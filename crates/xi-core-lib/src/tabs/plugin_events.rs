//! `impl CoreState` methods: plugin_events.
use super::*;

impl CoreState {
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

    pub(super) fn take_pending_plugin_commands(
        &mut self,
        plugin_name: &str,
    ) -> Vec<PendingPluginCommand> {
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

    pub(super) fn plugin_init_info(
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
