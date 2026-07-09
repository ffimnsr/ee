use log::{debug, error, warn};
use serde_json::{Value, json};
use xi_rope::LinesMetric;
use xi_rpc::{Error as RpcError, RemoteError};

use crate::line_offset::LineOffset;
use crate::plugins::rpc::{
    GetDiagnosticsResponse, GetSelectionsResponse, Hover, PluginBufferInfo, PluginNotification,
    PluginRequest, PluginUpdateAck, SelectionRange,
};
use crate::plugins::{Plugin, PluginCapability, PluginTerminationReason};
use crate::rpc::Position as ClientPosition;
use crate::tabs::PluginId;

use super::{EventContext, buffer_items_to_table};

impl<'a> EventContext<'a> {
    pub(super) fn dispatch_capability_command(
        &self,
        capability: PluginCapability,
        method: &str,
        params: &Value,
    ) {
        let mut dispatched = false;
        self.plugins
            .iter()
            .filter(|plugin| {
                plugin.manifest.has_capability(capability)
                    && plugin.manifest.supports_command(method)
            })
            .for_each(|plugin| {
                dispatched = true;
                plugin.dispatch_command(self.view_id, method, params);
            });

        if !dispatched {
            warn!("no running plugin registered {:?} command", method);
        }
    }

    /// Dispatches an incoming notification from a plugin (fire-and-forget).
    pub(crate) fn do_plugin_cmd(&mut self, plugin: PluginId, cmd: PluginNotification) {
        use self::PluginNotification::*;
        match cmd {
            Edit { edit } => {
                let ack = self.with_editor(|ed, _, _, _| ed.apply_plugin_edit(edit));
                if !ack.applied {
                    warn!("plugin edit rejected at revision {}: {:?}", ack.rev, ack.reason);
                }
            }
            Alert { msg } => self.client.alert(&msg),
            AddStatusItem { key, value, alignment } => {
                let plugin_name = self
                    .plugins
                    .iter()
                    .find(|p| p.id == plugin)
                    .map(|plugin| plugin.name.as_str())
                    .unwrap_or_else(|| {
                        warn!("status item update from unknown plugin {:?}", plugin);
                        "unknown-plugin"
                    });
                self.client.add_status_item(self.view_id, plugin_name, &key, &value, &alignment);
            }
            UpdateStatusItem { key, value } => {
                self.client.update_status_item(self.view_id, &key, &value)
            }
            UpdateAnnotations { start, len, spans, annotation_type, rev } => {
                self.with_editor(|ed, view, _, _| {
                    ed.update_annotations(view, plugin, start, len, spans, annotation_type, rev)
                })
            }
            UpdateDiagnostics { diagnostics } => {
                self.with_view(|view, _| view.update_diagnostics(plugin, diagnostics))
            }
            RemoveStatusItem { key } => self.client.remove_status_item(self.view_id, &key),
            ShowHover { request_id, result } => self.do_show_hover(request_id, result),
            ShowCompletions { items } => self.client.completions(self.view_id, &items),
            ShowCodeActions { actions } => self.client.code_actions(self.view_id, &actions),
            ShowLocations { title, locations } => {
                self.client.locations(self.view_id, &title, &locations)
            }
            ShowSymbols { title, symbols } => self.client.symbols(self.view_id, &title, &symbols),
        };
        self.after_edit(&plugin.to_string());
        self.render_if_needed();
    }

    pub(crate) fn do_plugin_cmd_sync(
        &mut self,
        _plugin: PluginId,
        cmd: PluginRequest,
    ) -> Result<Value, RemoteError> {
        use self::PluginRequest::*;
        match cmd {
            ApplyEdit { edit } => {
                Ok(json!(self.with_editor(|ed, _, _, _| ed.apply_plugin_edit(edit))))
            }
            LineCount => Ok(json!(self.editor.borrow().plugin_n_lines())),
            GetData { start, unit, max_size, rev } => {
                Ok(json!(self.editor.borrow().plugin_get_data(start, unit, max_size, rev)))
            }
            GetSelections => {
                let selections = self
                    .view
                    .borrow()
                    .sel_regions()
                    .iter()
                    .map(|region| SelectionRange { start: region.start, end: region.end })
                    .collect();
                Ok(json!(GetSelectionsResponse { selections }))
            }
            GetDiagnostics => Ok(json!(GetDiagnosticsResponse {
                diagnostics: self.view.borrow().get_diagnostics(),
            })),
            FormatDocument(..) => Err(RemoteError::custom(
                501,
                "document formatting is not implemented for plugins",
                None,
            )),
            GetCodeActions(..) => {
                Err(RemoteError::custom(501, "code actions are not implemented for plugins", None))
            }
        }
    }

    /// Builds a [`PluginBufferInfo`] snapshot describing the current buffer
    /// state for delivery to plugins during initialisation or restart.
    pub(crate) fn plugin_info(&mut self) -> PluginBufferInfo {
        let ed = self.editor.borrow();
        let nb_lines = ed.get_buffer().measure::<LinesMetric>() + 1;
        let views: Vec<crate::tabs::ViewId> = std::iter::once(&self.view)
            .chain(self.siblings.iter())
            .map(|v| v.borrow().get_view_id())
            .collect();

        let changes = buffer_items_to_table(self.config);
        let path = self.info.map(|info| info.path.to_owned());
        PluginBufferInfo::new(
            self.buffer_id,
            &views,
            ed.get_head_rev_token(),
            ed.get_buffer().len(),
            nb_lines,
            path,
            self.language.clone(),
            changes,
        )
    }

    /// Notifies the client that `plugin` has started for this view.
    pub(crate) fn plugin_started(&self, plugin: &Plugin) {
        self.client.plugin_started(self.view_id, &plugin.name)
    }

    /// Notifies the client that `plugin` has stopped.
    pub(crate) fn plugin_stopped(&mut self, plugin: &Plugin) {
        self.client.plugin_stopped(self.view_id, &plugin.name, 0);
    }

    pub(crate) fn plugin_terminated(&self, plugin_name: &str, reason: &PluginTerminationReason) {
        self.client.plugin_terminated(self.view_id, plugin_name, reason);
    }

    /// Handles the acknowledgement from a plugin after an update was delivered.
    pub(crate) fn do_plugin_update(&mut self, update: Result<Value, RpcError>) {
        match update.map(serde_json::from_value::<PluginUpdateAck>) {
            Ok(Ok(_)) => (),
            Ok(Err(err)) => error!("plugin response json err: {:?}", err),
            Err(err) => error!("plugin shutdown, do something {:?}", err),
        }
        self.editor.borrow_mut().dec_revs_in_flight();
    }

    /// Handles the response to a hover request from a plugin, forwarding the
    /// result to the client or logging an error on failure.
    pub(crate) fn do_plugin_hover(&mut self, request_id: usize, hover: Result<Value, RpcError>) {
        match hover.map(serde_json::from_value::<Hover>) {
            Ok(Ok(hover)) => self.do_show_hover(request_id, Ok(hover)),
            Ok(Err(err)) => error!("hover response json err: {:?}", err),
            Err(RpcError::RequestCancelled) => {
                debug!("hover request {} cancelled", request_id)
            }
            Err(RpcError::RemoteError(err)) => self.do_show_hover(request_id, Err(err)),
            Err(err) => warn!("hover request {} failed: {:?}", request_id, err),
        }
    }

    pub(crate) fn do_request_hover(&mut self, request_id: usize, position: Option<ClientPosition>) {
        use crate::plugins::PluginCapability;

        if let Some(position) = self.get_resolved_position(position) {
            let hover_plugins = self
                .plugins
                .iter()
                .filter(|plugin| plugin.manifest.has_capability(PluginCapability::Hover))
                .copied()
                .collect::<Vec<_>>();

            hover_plugins.into_iter().for_each(|plugin| {
                if let Some(previous_request) =
                    self.with_view(|view, _| view.take_pending_hover_request(plugin.id))
                {
                    plugin.cancel_request(previous_request);
                }

                let weak_core = self.weak_core.clone();
                let plugin_id = plugin.id;
                let view_id = self.view_id;
                let hover_request = plugin.request_hover(view_id, position, move |resp| {
                    weak_core.handle_plugin_hover(plugin_id, view_id, request_id, resp);
                });
                self.with_view(|view, _| {
                    view.replace_pending_hover_request(plugin_id, hover_request)
                });
            })
        }
    }

    fn do_show_hover(&mut self, request_id: usize, hover: Result<Hover, RemoteError>) {
        match hover {
            Ok(hover) => self.client.hover(self.view_id, request_id, hover.content),
            Err(err) => warn!("Hover Response from Client Error {:?}", err),
        }
    }

    /// Gives the requested position in UTF-8 offset format to be sent to plugin.
    /// If position is `None`, tries to get the current Caret Position instead.
    fn get_resolved_position(&mut self, position: Option<ClientPosition>) -> Option<usize> {
        position
            .and_then(|p| {
                match self
                    .with_view(|view, text| view.try_line_col_to_offset(text, p.line, p.column))
                {
                    Ok(offset) => Some(offset),
                    Err(err) => {
                        self.client.alert(format!("request_hover: {err}"));
                        None
                    }
                }
            })
            .or_else(|| self.view.borrow().get_caret_offset())
    }
}
