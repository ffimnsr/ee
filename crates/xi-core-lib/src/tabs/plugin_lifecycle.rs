use log::error;

use super::{CoreState, StopReason};
use crate::plugins::rpc::ClientPluginInfo;
use crate::plugins::{Plugin, PluginStartError, PluginStartErrorKind};

impl CoreState {
    /// Called from a plugin's thread after trying to start plugin process.
    pub(crate) fn plugin_connect(&mut self, plugin: Result<Plugin, PluginStartError>) {
        match plugin {
            Ok(plugin) => self.finish_plugin_connect(plugin),
            Err(err) => self.fail_plugin_connect(err),
        }
    }

    fn finish_plugin_connect(&mut self, plugin: Plugin) {
        self.launching_plugins.remove(&plugin.name);
        let pending_commands = self.take_pending_plugin_commands(&plugin.name);
        let init_info = self.plugin_init_info(&plugin, &pending_commands);
        let plugin_config =
            self.config_manager.get_plugin_config(&plugin.name).cloned().unwrap_or_default();
        let should_shutdown =
            pending_commands.iter().any(|command| command.shutdown_after_dispatch)
                || (plugin.is_single_invocation() && !pending_commands.is_empty());
        let plugin_id = plugin.id;
        let plugin_name = plugin.name.clone();

        plugin.initialize(init_info, &plugin_config);
        pending_commands.iter().for_each(|command| {
            plugin.dispatch_command(command.view_id, &command.method, &command.params);
        });
        self.plugin_restart_state.entry(plugin_name.clone()).or_default().last_start =
            Some(std::time::Instant::now());
        self.running_plugins.push(plugin);
        self.notify_views_plugin_started(&plugin_name);

        if should_shutdown {
            self.begin_plugin_shutdown(plugin_id, StopReason::SingleInvocation);
        }
    }

    fn fail_plugin_connect(&mut self, err: PluginStartError) {
        self.launching_plugins.remove(&err.name);
        error!("failed to start plugin {}: {:?}", err.name, err.source);
        let detail = match err.source {
            PluginStartErrorKind::Io(source) => source.to_string(),
            PluginStartErrorKind::UnsupportedTransport(transport) => {
                format!("unsupported transport {transport:?}")
            }
            PluginStartErrorKind::Sandbox(detail) | PluginStartErrorKind::Wasm(detail) => detail,
        };
        self.peer.alert(format!("failed to start plugin {}: {}", err.name, detail));
        self.schedule_plugin_restart(&err.name);
    }

    fn notify_views_plugin_started(&self, plugin_name: &str) {
        for (view_id, view) in &self.views {
            if self.pending_views.iter().any(|(pending_id, _)| pending_id == view_id) {
                continue;
            }

            let buffer_id = view.borrow().get_buffer_id();
            let language = self.config_manager.get_buffer_language(buffer_id);
            let available_plugins = self
                .running_plugins
                .iter()
                .filter(|plugin| plugin.receives_updates_for(&language))
                .map(|plugin| ClientPluginInfo { name: plugin.name.clone(), running: true })
                .collect::<Vec<_>>();

            if available_plugins.iter().any(|plugin| plugin.name == plugin_name) {
                self.peer.plugin_started(*view_id, plugin_name);
            }
            self.peer.available_plugins(*view_id, &available_plugins);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::Value;
    use xi_rpc::{Callback, Error, Peer, RequestId, RpcPeer};

    use super::CoreState;
    use crate::core::dummy_weak_core;
    use crate::plugins::test_support::test_plugin;

    #[derive(Clone, Default)]
    struct RecordingPeer {
        notifications: Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl RecordingPeer {
        fn clear(&self) {
            self.notifications.lock().expect("notification lock").clear();
        }

        fn notifications(&self) -> Vec<(String, Value)> {
            self.notifications.lock().expect("notification lock").clone()
        }
    }

    impl Peer for RecordingPeer {
        fn box_clone(&self) -> Box<dyn Peer> {
            Box::new(self.clone())
        }

        fn send_rpc_notification(&self, method: &str, params: &Value) {
            self.notifications
                .lock()
                .expect("notification lock")
                .push((method.to_string(), params.clone()));
        }

        fn send_rpc_request_async(
            &self,
            _method: &str,
            _params: &Value,
            _callback: Box<dyn Callback>,
        ) -> RequestId {
            RequestId::Number(0)
        }

        fn send_rpc_request(&self, _method: &str, _params: &Value) -> Result<Value, Error> {
            Ok(Value::Null)
        }

        fn send_rpc_request_timeout(
            &self,
            _method: &str,
            _params: &Value,
            _timeout: Duration,
        ) -> Result<Value, Error> {
            Ok(Value::Null)
        }

        fn cancel_rpc_request(&self, _id: RequestId) -> bool {
            false
        }

        fn request_is_pending(&self) -> bool {
            false
        }

        fn schedule_idle(&self, _token: usize) {}

        fn schedule_timer(&self, _after: Instant, _token: usize) {}

        fn cancel_timer(&self, _token: usize) -> bool {
            false
        }

        fn request_shutdown(&self) {}
    }

    #[test]
    fn late_plugin_connect_notifies_existing_view() {
        let peer = RecordingPeer::default();
        let rpc_peer: RpcPeer = Box::new(peer.clone());
        let mut core = CoreState::new(&rpc_peer, None, None);
        core.self_ref = Some(dummy_weak_core());
        let view_id = core.do_new_view(None).expect("new view should open");
        core.finalize_new_views();
        peer.clear();

        core.plugin_connect(Ok(test_plugin("late-plugin")));

        let notifications = peer.notifications();
        assert!(notifications.iter().any(|(method, params)| {
            method == "plugin_started"
                && params.get("view_id") == Some(&view_id)
                && params.get("plugin").and_then(Value::as_str) == Some("late-plugin")
        }));
        assert!(notifications.iter().any(|(method, params)| {
            method == "available_plugins"
                && params.get("view_id") == Some(&view_id)
                && params.get("plugins").and_then(Value::as_array).is_some_and(|plugins| {
                    plugins.iter().any(|plugin| {
                        plugin.get("name").and_then(Value::as_str) == Some("late-plugin")
                            && plugin.get("running").and_then(Value::as_bool) == Some(true)
                    })
                })
        }));
    }
}
