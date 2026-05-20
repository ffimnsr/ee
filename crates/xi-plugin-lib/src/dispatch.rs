// Copyright 2018 The xi-editor Authors.
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

use std::collections::HashMap;
use std::ops::DerefMut;
use std::path::PathBuf;

use log::{info, warn};
use serde_json::{self, Value, json};

use crate::core_proxy::CoreProxy;
use xi_core_lib::plugin_rpc::{
    HostNotification, HostRequest, PluginBufferInfo, PluginUpdate, PluginUpdateAck,
    ProtocolCapability,
};
use xi_core_lib::tracing_support;
use xi_core_lib::{ConfigTable, LanguageId, PluginPid, ViewId};
use xi_rpc::{Handler as RpcHandler, OptionExt, RemoteError, ResultExt, RpcCtx};

use super::{Plugin, View};

/// Handles raw RPCs from core, updating state and forwarding calls
/// to the plugin,
pub struct Dispatcher<P: Plugin, H: DerefMut<Target = P>> {
    buffers: HashMap<ViewId, View<P::Cache>>,
    view_to_buffer: HashMap<ViewId, ViewId>,
    pid: Option<PluginPid>,
    plugin: H,
}

impl<P: Plugin, H: DerefMut<Target = P>> Dispatcher<P, H> {
    fn supported_protocol_capabilities() -> [ProtocolCapability; 3] {
        [
            ProtocolCapability::CoreCapabilityNegotiation,
            ProtocolCapability::GracefulShutdown,
            ProtocolCapability::RestartBackoff,
        ]
    }

    pub(crate) fn new(plugin: H) -> Self {
        Dispatcher { buffers: HashMap::new(), view_to_buffer: HashMap::new(), pid: None, plugin }
    }

    fn warn_missing_view(pid: Option<PluginPid>, method: &str, view_id: ViewId) {
        warn!("{:?} missing {:?} for {:?}", pid, view_id, method);
    }

    fn with_view_mut<R>(
        &mut self,
        method: &str,
        view_id: ViewId,
        f: impl FnOnce(&mut P, &mut View<P::Cache>) -> R,
    ) -> Option<R> {
        let pid = self.pid;
        let Some(buffer_key) = self.view_to_buffer.get(&view_id).copied() else {
            Self::warn_missing_view(pid, method, view_id);
            return None;
        };
        let Some(view) = self.buffers.get_mut(&buffer_key) else {
            Self::warn_missing_view(pid, method, view_id);
            return None;
        };
        if view.set_active_view(view_id).is_err() {
            Self::warn_missing_view(pid, method, view_id);
            return None;
        }

        let plugin = &mut *self.plugin;
        Some(f(plugin, view))
    }

    fn with_view_mut_or_error<R>(
        &mut self,
        method: &str,
        view_id: ViewId,
        f: impl FnOnce(&mut P, &mut View<P::Cache>) -> R,
    ) -> Result<R, RemoteError> {
        self.with_view_mut(method, view_id, f).ok_or_not_found("missing view")
    }

    fn do_initialize(
        &mut self,
        ctx: &RpcCtx,
        plugin_id: PluginPid,
        buffers: Vec<PluginBufferInfo>,
        plugin_config: ConfigTable,
        protocol_version: u32,
        core_capabilities: Vec<ProtocolCapability>,
    ) {
        assert!(self.pid.is_none(), "initialize rpc received with existing pid");
        info!("Initializing plugin {:?}", plugin_id);
        self.pid = Some(plugin_id);

        let core_proxy = CoreProxy::new(
            self.pid.unwrap(),
            ctx,
            protocol_version,
            core_capabilities
                .into_iter()
                .filter(|capability| Self::supported_protocol_capabilities().contains(capability)),
        );
        self.plugin.initialize(core_proxy);
        self.plugin.plugin_config_changed(&plugin_config);

        self.do_new_buffer(ctx, buffers);
    }

    fn do_plugin_config_changed(&mut self, changes: &ConfigTable) {
        self.plugin.plugin_config_changed(changes);
    }

    fn do_did_save(&mut self, view_id: ViewId, path: PathBuf) {
        let _ = self.with_view_mut("did_save", view_id, |plugin, view| {
            let prev_path = view.path.take();
            view.path = Some(path);
            plugin.did_save(view, prev_path.as_deref());
        });
    }

    fn do_config_changed(&mut self, view_id: ViewId, changes: &ConfigTable) {
        let _ = self.with_view_mut("config_changed", view_id, |plugin, view| {
            let mut next_config_table = view.config_table.clone();
            for (key, value) in changes.iter() {
                next_config_table.insert(key.to_owned(), value.to_owned());
            }
            match serde_json::from_value(Value::Object(next_config_table.clone())) {
                Ok(config) => {
                    view.config_table = next_config_table;
                    view.config = config;
                    plugin.config_changed(view, changes);
                }
                Err(source) => warn!(
                    "failed to apply config update for {:?}: {:?}",
                    view_id,
                    super::Error::ConfigDeserialization { context: "config update", source }
                ),
            }
        });
    }

    fn do_language_changed(&mut self, view_id: ViewId, new_lang: LanguageId) {
        let _ = self.with_view_mut("language_changed", view_id, |plugin, view| {
            let old_lang = view.language_id.clone();
            view.set_language(new_lang);
            plugin.language_changed(view, old_lang);
        });
    }

    fn do_custom_command(&mut self, view_id: ViewId, method: &str, params: Value) {
        let _ = self.with_view_mut(method, view_id, |plugin, view| {
            plugin.custom_command(view, method, params);
        });
    }

    fn do_new_buffer(&mut self, ctx: &RpcCtx, buffers: Vec<PluginBufferInfo>) {
        let plugin_id = self.pid.unwrap();
        buffers.into_iter().for_each(|info| {
            let existing_buffer_keys = info
                .views
                .iter()
                .filter_map(|view_id| self.view_to_buffer.get(view_id).copied())
                .collect::<Vec<_>>();
            if let Some(buffer_key) = existing_buffer_keys.first().copied() {
                let Some(view) = self.buffers.get_mut(&buffer_key) else {
                    warn!("failed to merge plugin view for {:?}: missing existing buffer state", plugin_id);
                    return;
                };

                let previous_active = view.get_id();
                for view_id in info.views {
                    if !view.add_view_id(view_id) {
                        continue;
                    }
                    self.view_to_buffer.insert(view_id, buffer_key);
                    if view.set_active_view(view_id).is_ok() {
                        self.plugin.new_view(view);
                    }
                }
                let _ = view.set_active_view(previous_active);
                return;
            }

            match View::new(ctx.get_peer().clone(), plugin_id, info) {
                Ok(mut view) => {
                    let primary_view_id = view.primary_view_id();
                    if view.get_view_ids().iter().any(|view_id| self.view_to_buffer.contains_key(view_id)) {
                        let duplicate_view_ids = view
                            .get_view_ids()
                            .iter()
                            .filter(|view_id| self.view_to_buffer.contains_key(view_id))
                            .copied()
                            .collect::<Vec<_>>();
                        warn!(
                            "failed to create plugin view for {:?}: duplicate view id in buffer state; incoming={:?}; duplicates={:?}; existing_keys={:?}",
                            plugin_id,
                            view.get_view_ids(),
                            duplicate_view_ids,
                            existing_buffer_keys,
                        );
                        return;
                    }

                    let incoming_view_ids = view.get_view_ids().to_vec();
                    for view_id in &incoming_view_ids {
                        if view.set_active_view(*view_id).is_ok() {
                            self.plugin.new_view(&mut view);
                        }
                    }
                    let _ = view.set_active_view(primary_view_id);

                    for view_id in incoming_view_ids {
                        self.view_to_buffer.insert(view_id, primary_view_id);
                    }
                    self.buffers.insert(primary_view_id, view);
                }
                Err(err) => warn!("failed to create plugin view for {:?}: {:?}", plugin_id, err),
            }
        });
    }

    fn do_close(&mut self, view_id: ViewId) {
        let pid = self.pid;
        let Some(buffer_key) = self.view_to_buffer.get(&view_id).copied() else {
            Self::warn_missing_view(pid, "close", view_id);
            return;
        };

        let (remaining_view_ids, new_primary) = {
            let Some(v) = self.buffers.get_mut(&buffer_key) else {
                Self::warn_missing_view(pid, "close", view_id);
                return;
            };
            if v.set_active_view(view_id).is_err() {
                Self::warn_missing_view(pid, "close", view_id);
                return;
            }
            self.plugin.did_close(v);
            let _ = v.remove_view_id(view_id);
            (v.get_view_ids().to_vec(), v.get_view_ids().first().copied())
        };

        self.view_to_buffer.remove(&view_id);

        if remaining_view_ids.is_empty() {
            self.buffers.remove(&buffer_key);
            return;
        }

        if let Some(new_primary) = new_primary
            && new_primary != buffer_key
            && let Some(view) = self.buffers.remove(&buffer_key)
        {
            for remaining_view_id in &remaining_view_ids {
                self.view_to_buffer.insert(*remaining_view_id, new_primary);
            }
            self.buffers.insert(new_primary, view);
        }
    }

    fn do_shutdown(&mut self, ctx: &RpcCtx) {
        info!("shutting down rust plugin {:?}", self.pid);
        self.plugin.shutdown();
        ctx.request_shutdown();
    }

    fn do_get_hover(
        &mut self,
        view_id: ViewId,
        position: usize,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Value, RemoteError> {
        self.with_view_mut_or_error("get_hover", view_id, |plugin, view| {
            plugin.get_hover(view, position, cancel).map(|hover| json!(hover))
        })?
    }

    fn do_tracing_config(&mut self, enabled: bool) {
        if enabled {
            tracing_support::set_enabled(true);
            info!("Enabling tracing in global plugin {:?}", self.pid);
            tracing::trace!(name: "enable tracing", categories = "plugin");
        } else {
            tracing_support::set_enabled(false);
            info!("Disabling tracing in global plugin {:?}", self.pid);
        }
    }

    fn do_update(&mut self, update: PluginUpdate) -> Result<Value, RemoteError> {
        let _t = tracing::trace_span!("Dispatcher::do_update", categories = "plugin").entered();
        let PluginUpdate {
            view_id,
            delta,
            new_len,
            new_line_count,
            rev,
            undo_group,
            edit_type,
            author,
        } = update;
        self.with_view_mut_or_error("update", view_id, |plugin, view| {
            view.update(delta.as_ref(), new_len, new_line_count, rev, undo_group);
            plugin.update(view, delta.as_ref(), edit_type, author);
        })?;

        Ok(json!(PluginUpdateAck { view_id, rev }))
    }

    fn do_collect_trace(&self) -> Result<Value, RemoteError> {
        tracing_support::collect_json()
            .map_err_remote(0, |err| format!("Could not serialize trace: {err:?}"))
    }
}

impl<P: Plugin, H: DerefMut<Target = P>> RpcHandler for Dispatcher<P, H> {
    type Notification = HostNotification;
    type Request = HostRequest;

    fn handle_notification(&mut self, ctx: &RpcCtx, rpc: Self::Notification) {
        use self::HostNotification::*;
        let _t = tracing::trace_span!("Dispatcher::handle_notif", categories = "plugin").entered();
        match rpc {
            Initialize {
                plugin_id,
                buffer_info,
                plugin_config,
                protocol_version,
                core_capabilities,
            } => self.do_initialize(
                ctx,
                plugin_id,
                buffer_info,
                plugin_config,
                protocol_version,
                core_capabilities,
            ),
            PluginConfigChanged { changes } => self.do_plugin_config_changed(&changes),
            DidSave { view_id, path } => self.do_did_save(view_id, path),
            ConfigChanged { view_id, changes } => self.do_config_changed(view_id, &changes),
            NewBuffer { buffer_info } => self.do_new_buffer(ctx, buffer_info),
            DidClose { view_id } => self.do_close(view_id),
            Shutdown(..) => self.do_shutdown(ctx),
            TracingConfig { enabled } => self.do_tracing_config(enabled),
            LanguageChanged { view_id, new_lang } => self.do_language_changed(view_id, new_lang),
            CustomCommand { view_id, method, params } => {
                self.do_custom_command(view_id, &method, params)
            }
            Ping(..) => (),
        }
    }

    fn handle_request(
        &mut self,
        _ctx: &RpcCtx,
        rpc: Self::Request,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Value, RemoteError> {
        use self::HostRequest::*;
        let _t =
            tracing::trace_span!("Dispatcher::handle_request", categories = "plugin").entered();
        match rpc {
            Update(params) => self.do_update(params),
            GetHover { view_id, position } => self.do_get_hover(view_id, position, cancel),
            CollectTrace(..) => self.do_collect_trace(),
        }
    }

    fn idle(&mut self, _ctx: &RpcCtx, token: usize) {
        let _t = tracing::trace_span!("Dispatcher::idle", categories = "plugin", token).entered();
        let view_id: ViewId = token.into();
        let _ = self.with_view_mut("idle", view_id, |plugin, view| {
            plugin.idle(view);
        });
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;

    use serde_json::{Value, json};
    use xi_rpc::test_utils::{DummyPeer, make_reader};
    use xi_rpc::{Handler, NewlineWriter, RpcCtx, RpcLoop};

    use super::*;
    use crate::ChunkCache;
    use xi_rope::RopeDelta;

    struct ConfigPlugin {
        changed_calls: usize,
        new_view_ids: Vec<(ViewId, Vec<ViewId>)>,
        closed_view_ids: Vec<(ViewId, Vec<ViewId>)>,
    }

    impl Plugin for ConfigPlugin {
        type Cache = ChunkCache;

        fn update(
            &mut self,
            _view: &mut View<Self::Cache>,
            _delta: Option<&RopeDelta>,
            _edit_type: String,
            _author: String,
        ) {
        }

        fn did_save(&mut self, _view: &mut View<Self::Cache>, _old_path: Option<&Path>) {}

        fn did_close(&mut self, view: &View<Self::Cache>) {
            self.closed_view_ids.push((view.get_id(), view.get_view_ids().to_vec()));
        }

        fn new_view(&mut self, view: &mut View<Self::Cache>) {
            self.new_view_ids.push((view.get_id(), view.get_view_ids().to_vec()));
        }

        fn config_changed(&mut self, _view: &mut View<Self::Cache>, _changes: &ConfigTable) {
            self.changed_calls += 1;
        }
    }

    #[derive(Default)]
    struct InitOrderPlugin {
        events: Vec<&'static str>,
        plugin_config: Option<ConfigTable>,
        new_view_paths: Vec<Option<PathBuf>>,
    }

    impl Plugin for InitOrderPlugin {
        type Cache = ChunkCache;

        fn initialize(&mut self, _core: CoreProxy) {
            self.events.push("initialize");
        }

        fn plugin_config_changed(&mut self, changes: &ConfigTable) {
            self.events.push("plugin_config_changed");
            self.plugin_config = Some(changes.clone());
        }

        fn update(
            &mut self,
            _view: &mut View<Self::Cache>,
            _delta: Option<&RopeDelta>,
            _edit_type: String,
            _author: String,
        ) {
        }

        fn did_save(&mut self, _view: &mut View<Self::Cache>, _old_path: Option<&Path>) {}

        fn did_close(&mut self, _view: &View<Self::Cache>) {}

        fn new_view(&mut self, view: &mut View<Self::Cache>) {
            self.events.push("new_view");
            self.new_view_paths.push(view.get_path().map(PathBuf::from));
            assert!(self.plugin_config.is_some(), "plugin config should arrive before new_view");
        }

        fn config_changed(&mut self, _view: &mut View<Self::Cache>, _changes: &ConfigTable) {}
    }

    struct CtxRunner<F> {
        callback: Option<F>,
    }

    impl<F> Handler for CtxRunner<F>
    where
        F: FnOnce(&RpcCtx),
    {
        type Notification = serde_json::Value;
        type Request = serde_json::Value;

        fn handle_notification(&mut self, ctx: &RpcCtx, _rpc: Self::Notification) {
            if let Some(callback) = self.callback.take() {
                callback(ctx);
            }
            ctx.get_peer().request_shutdown();
        }

        fn handle_request(
            &mut self,
            _ctx: &RpcCtx,
            _rpc: Self::Request,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<serde_json::Value, xi_rpc::RemoteError> {
            Ok(Value::Null)
        }
    }

    fn with_rpc_ctx(f: impl FnOnce(&RpcCtx)) {
        let mut handler = CtxRunner { callback: Some(f) };
        let mut loop_ = RpcLoop::new(NewlineWriter::new(std::io::sink()));
        let reader = make_reader(r#"{"method":"ping","params":{}}"#);
        loop_.mainloop(|| reader, &mut handler).expect("test rpc loop should exit cleanly");
    }

    fn valid_config() -> serde_json::Map<String, Value> {
        json!({
            "line_ending": "\n",
            "tab_size": 4,
            "translate_tabs_to_spaces": false,
            "use_tab_stops": true,
            "font_face": "monospace",
            "font_size": 14.0,
            "auto_indent": true,
            "scroll_past_end": false,
            "wrap_width": 0,
            "word_wrap": false,
            "autodetect_whitespace": true,
            "surrounding_pairs": [["(", ")"]],
            "save_with_newline": true
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    fn buffer_info(config: serde_json::Map<String, Value>) -> PluginBufferInfo {
        serde_json::from_value(json!({
            "buffer_id": 1,
            "views": ["view-id-1"],
            "rev": 7,
            "buf_size": 12,
            "nb_lines": 1,
            "path": null,
            "syntax": "plain_text",
            "config": config,
        }))
        .unwrap()
    }

    fn multi_view_buffer_info(config: serde_json::Map<String, Value>) -> PluginBufferInfo {
        serde_json::from_value(json!({
            "buffer_id": 1,
            "views": ["view-id-1", "view-id-2"],
            "rev": 7,
            "buf_size": 12,
            "nb_lines": 1,
            "path": null,
            "syntax": "plain_text",
            "config": config,
        }))
        .unwrap()
    }

    fn path_buffer_info(path: &str, config: serde_json::Map<String, Value>) -> PluginBufferInfo {
        serde_json::from_value(json!({
            "buffer_id": 1,
            "views": ["view-id-1"],
            "rev": 7,
            "buf_size": 12,
            "nb_lines": 1,
            "path": path,
            "syntax": "plain_text",
            "config": config,
        }))
        .unwrap()
    }

    #[test]
    fn invalid_config_update_keeps_previous_config() {
        let mut plugin = ConfigPlugin {
            changed_calls: 0,
            new_view_ids: Vec::new(),
            closed_view_ids: Vec::new(),
        };
        let mut dispatcher = Dispatcher::new(&mut plugin);
        let plugin_id = serde_json::from_value(json!(8)).unwrap();
        let view_id: ViewId = 1usize.into();
        let view = View::new(Box::new(DummyPeer), plugin_id, buffer_info(valid_config()))
            .expect("valid config should build view");

        dispatcher.view_to_buffer.insert(view_id, view.primary_view_id());
        dispatcher.buffers.insert(view.primary_view_id(), view);
        dispatcher.pid = Some(plugin_id);
        dispatcher
            .do_config_changed(view_id, &json!({ "tab_size": 0 }).as_object().cloned().unwrap());

        assert_eq!(dispatcher.plugin.changed_calls, 0);
        assert_eq!(dispatcher.buffers.get(&view_id).unwrap().config.tab_size, 4);
    }

    #[test]
    fn multi_view_buffers_dispatch_lifecycle_per_view() {
        let mut plugin = ConfigPlugin {
            changed_calls: 0,
            new_view_ids: Vec::new(),
            closed_view_ids: Vec::new(),
        };
        let mut dispatcher = Dispatcher::new(&mut plugin);
        let plugin_id = serde_json::from_value(json!(8)).unwrap();
        let view =
            View::new(Box::new(DummyPeer), plugin_id, multi_view_buffer_info(valid_config()))
                .expect("valid config should build view");
        let primary = view.primary_view_id();
        let view_ids = view.get_view_ids().to_vec();

        for view_id in &view_ids {
            dispatcher.view_to_buffer.insert(*view_id, primary);
        }
        dispatcher.buffers.insert(primary, view);
        dispatcher.pid = Some(plugin_id);

        for view_id in &view_ids {
            let _ = dispatcher.with_view_mut("new_view", *view_id, |plugin, view| {
                plugin.new_view(view);
            });
        }
        dispatcher.do_close(1usize.into());
        dispatcher.do_close(2usize.into());

        assert_eq!(dispatcher.plugin.new_view_ids.len(), 2);
        assert_eq!(dispatcher.plugin.new_view_ids[0].0, 1usize.into());
        assert_eq!(dispatcher.plugin.new_view_ids[1].0, 2usize.into());
        assert_eq!(dispatcher.plugin.new_view_ids[0].1, vec![1usize.into(), 2usize.into()]);
        assert_eq!(dispatcher.plugin.closed_view_ids.len(), 2);
        assert_eq!(dispatcher.plugin.closed_view_ids[0].0, 1usize.into());
        assert_eq!(dispatcher.plugin.closed_view_ids[1].0, 2usize.into());
        assert!(dispatcher.buffers.is_empty());
        assert!(dispatcher.view_to_buffer.is_empty());
    }

    #[test]
    fn initialize_delivers_plugin_config_before_new_view() {
        let mut plugin = InitOrderPlugin::default();
        let mut dispatcher = Dispatcher::new(&mut plugin);
        let plugin_id = serde_json::from_value(json!(9)).unwrap();
        let plugin_config = json!({
            "language_config": {
                "gleam": {
                    "extensions": ["gleam"]
                }
            }
        })
        .as_object()
        .cloned()
        .unwrap();

        with_rpc_ctx(|ctx| {
            dispatcher.handle_notification(
                ctx,
                HostNotification::Initialize {
                    plugin_id,
                    buffer_info: vec![path_buffer_info("/tmp/sample.gleam", valid_config())],
                    plugin_config: plugin_config.clone(),
                    protocol_version: 1,
                    core_capabilities: Vec::new(),
                },
            );
        });

        assert_eq!(
            dispatcher.plugin.events,
            vec!["initialize", "plugin_config_changed", "new_view"]
        );
        assert_eq!(dispatcher.plugin.plugin_config, Some(plugin_config));
        assert_eq!(
            dispatcher.plugin.new_view_paths,
            vec![Some(PathBuf::from("/tmp/sample.gleam"))]
        );
    }

    #[test]
    fn duplicate_new_buffer_for_initialized_view_is_ignored() {
        let mut plugin = ConfigPlugin {
            changed_calls: 0,
            new_view_ids: Vec::new(),
            closed_view_ids: Vec::new(),
        };
        let mut dispatcher = Dispatcher::new(&mut plugin);

        with_rpc_ctx(|ctx| {
            dispatcher.do_initialize(
                ctx,
                serde_json::from_value(json!(9)).unwrap(),
                vec![buffer_info(valid_config())],
                json!({}).as_object().cloned().unwrap(),
                1,
                Vec::new(),
            );
            dispatcher.do_new_buffer(ctx, vec![buffer_info(valid_config())]);
        });

        assert_eq!(dispatcher.plugin.new_view_ids.len(), 1);
        assert_eq!(dispatcher.plugin.new_view_ids[0].0, 1usize.into());
    }

    #[test]
    fn overlapping_new_buffer_adds_missing_view_ids() {
        let mut plugin = ConfigPlugin {
            changed_calls: 0,
            new_view_ids: Vec::new(),
            closed_view_ids: Vec::new(),
        };
        let mut dispatcher = Dispatcher::new(&mut plugin);

        with_rpc_ctx(|ctx| {
            dispatcher.do_initialize(
                ctx,
                serde_json::from_value(json!(9)).unwrap(),
                vec![buffer_info(valid_config())],
                json!({}).as_object().cloned().unwrap(),
                1,
                Vec::new(),
            );
            dispatcher.do_new_buffer(ctx, vec![multi_view_buffer_info(valid_config())]);
        });

        assert_eq!(dispatcher.plugin.new_view_ids.len(), 2);
        assert_eq!(dispatcher.plugin.new_view_ids[0].0, 1usize.into());
        assert_eq!(dispatcher.plugin.new_view_ids[1].0, 2usize.into());
    }
}
