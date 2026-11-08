//! JSON dispatch for service calls initiated by native plugins.
//!
//! The editor host resolves `editor.<service>@1` plus a method name into a
//! typed Rust call. Permissions are checked before deserialization so a denied
//! plugin cannot probe argument shapes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::{PluginId, ServiceId};
use ee_plugin_api::model::{BufferId, ViewId};
use serde_json::Value;

use crate::plugin_runtime::commands::EditorCommandRegistry;
use crate::plugin_runtime::services::{
    ConfigService, DecorationService, DocumentEditService, DocumentService, FsService, LspService,
    SelectionService, SyntaxService, TaskService, UiService, ViewsService,
    document::{DeleteRequest, InsertRequest, ReplaceRangeRequest},
    fs::FsScope,
    lsp::LspRequest,
    ui::UiRenderRequest,
};

/// Routes external `plugin/request` calls to editor services.
#[derive(Clone)]
pub struct EditorServiceRouter {
    pub document: Arc<DocumentService>,
    pub document_edit: Arc<DocumentEditService>,
    pub views: Arc<ViewsService>,
    pub selection: Arc<SelectionService>,
    pub commands: Arc<EditorCommandRegistry>,
    pub syntax: Arc<SyntaxService>,
    pub lsp: Arc<LspService>,
    pub decorations: Arc<DecorationService>,
    pub ui: Arc<UiService>,
    pub tasks: Arc<TaskService>,
    pub fs: Arc<FsService>,
    pub config: Arc<ConfigService>,
    permissions: Arc<RwLock<BTreeMap<String, BTreeSet<String>>>>,
}

impl EditorServiceRouter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        document: Arc<DocumentService>,
        document_edit: Arc<DocumentEditService>,
        views: Arc<ViewsService>,
        selection: Arc<SelectionService>,
        commands: Arc<EditorCommandRegistry>,
        syntax: Arc<SyntaxService>,
        lsp: Arc<LspService>,
        decorations: Arc<DecorationService>,
        ui: Arc<UiService>,
        tasks: Arc<TaskService>,
        fs: Arc<FsService>,
        config: Arc<ConfigService>,
    ) -> Self {
        Self {
            document,
            document_edit,
            views,
            selection,
            commands,
            syntax,
            lsp,
            decorations,
            ui,
            tasks,
            fs,
            config,
            permissions: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    pub fn set_permissions(&self, plugin: &PluginId, permissions: BTreeSet<String>) {
        self.permissions
            .write()
            .expect("permission lock poisoned")
            .insert(plugin.as_str().to_owned(), permissions);
    }

    pub fn clear_permissions(&self) {
        self.permissions.write().expect("permission lock poisoned").clear();
    }

    pub fn permissions_for(&self, plugin: &PluginId) -> Option<BTreeSet<String>> {
        self.permissions.read().expect("permission lock poisoned").get(plugin.as_str()).cloned()
    }

    fn authorize(&self, plugin: &PluginId, permission: &str) -> Result<(), PluginError> {
        let permissions = self.permissions.read().expect("permission lock poisoned");
        let Some(set) = permissions.get(plugin.as_str()) else {
            return Err(PluginError::Permission { permission: permission.to_owned() });
        };
        if set.contains(permission) {
            Ok(())
        } else {
            Err(PluginError::Permission { permission: permission.to_owned() })
        }
    }

    /// Dispatch one service method.
    pub fn dispatch(
        &self,
        plugin: &PluginId,
        service: &ServiceId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        match service.as_str() {
            "editor.document@1" => self.document_call(plugin, method, params),
            "editor.document.edit@1" => self.edit_call(plugin, method, params),
            "editor.views@1" => self.views_call(plugin, method, params),
            "editor.selection@1" => self.selection_call(plugin, method, params),
            "editor.commands@1" => self.commands_call(plugin, method, params),
            "editor.syntax@1" => self.syntax_call(plugin, method, params),
            "editor.lsp@1" => self.lsp_call(plugin, method, params),
            "editor.decorations@1" => self.decorations_call(plugin, method, params),
            "editor.ui@1" => self.ui_call(plugin, method, params),
            "editor.tasks@1" => self.tasks_call(plugin, method, params),
            "editor.fs@1" => self.fs_call(plugin, method, params),
            "editor.config@1" => self.config_call(plugin, method, params),
            _other => Err(PluginError::Service(ee_plugin_api::service::ServiceError::Missing {
                id: service.clone(),
            })),
        }
    }

    fn document_call(
        &self,
        plugin: &PluginId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        self.authorize(plugin, "editor.document.read")?;
        match method {
            "snapshot" => json(self.document.snapshot(buffer(&params)?)?),
            "revision" => json(self.document.revision(buffer(&params)?)?),
            "length" => json(self.document.length(buffer(&params)?)?),
            "line_count" => json(self.document.line_count(buffer(&params)?)?),
            "text_range" => {
                let start = usize_param(&params, "start")?;
                let end = usize_param(&params, "end")?;
                json(
                    self.document
                        .text_range(buffer(&params)?, ee_plugin_api::model::Range { start, end })?,
                )
            }
            "document_text" => {
                let max = params.get("max_bytes").and_then(Value::as_u64).unwrap_or(1024 * 1024);
                json(self.document.document_text(buffer(&params)?, max as usize)?)
            }
            other => Err(unsupported_method("editor.document", other)),
        }
    }

    fn edit_call(
        &self,
        plugin: &PluginId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        self.authorize(plugin, "editor.document.write")?;
        match method {
            "apply" => json(self.document_edit.apply(from_value(params)?)?),
            "replace_range" => {
                json(self.document_edit.replace_range(from_value::<ReplaceRangeRequest>(params)?)?)
            }
            "insert" => json(self.document_edit.insert(from_value::<InsertRequest>(params)?)?),
            "delete" => json(self.document_edit.delete(from_value::<DeleteRequest>(params)?)?),
            other => Err(unsupported_method("editor.document.edit", other)),
        }
    }

    fn views_call(
        &self,
        plugin: &PluginId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        self.authorize(plugin, "editor.views.read")?;
        match method {
            "views_for_buffer" => json(self.views.views_for_buffer(buffer(&params)?)),
            "active_view" => json(self.views.active_view()),
            "viewport" => json(self.views.viewport(view(&params)?)?),
            "view_buffer" => json(self.views.view_buffer(view(&params)?)),
            "view_language" => json(self.views.view_language(view(&params)?)),
            "reveal" => {
                let line = usize_param(&params, "line")?;
                let column = usize_param(&params, "column")?;
                json(self.views.reveal(view(&params)?, line, column)?)
            }
            "scroll_to" => {
                let request = ee_plugin_api::effect::ScrollRequest {
                    view_id: view(&params)?,
                    line: usize_param(&params, "line")?,
                    column: usize_param(&params, "column")?,
                };
                json(self.views.scroll_to(request)?)
            }
            other => Err(unsupported_method("editor.views", other)),
        }
    }

    fn selection_call(
        &self,
        plugin: &PluginId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        match method {
            "get" => {
                self.authorize(plugin, "editor.selection.read")?;
                json(self.selection.get(view(&params)?)?)
            }
            "set" => {
                self.authorize(plugin, "editor.selection.write")?;
                let request = ee_plugin_api::effect::SelectionRequest {
                    view_id: view(&params)?,
                    rev: params.get("rev").and_then(Value::as_u64).unwrap_or(0),
                    selections: from_value(
                        params.get("selections").cloned().unwrap_or(Value::Array(Vec::new())),
                    )?,
                };
                json(self.selection.set(request)?)
            }
            other => Err(unsupported_method("editor.selection", other)),
        }
    }

    fn commands_call(
        &self,
        plugin: &PluginId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        self.authorize(plugin, "editor.commands.invoke")?;
        match method {
            "list" => json(self.commands.ids()),
            "dispatch" => {
                let id = from_value::<ee_plugin_api::ids::CommandId>(
                    params.get("id").cloned().ok_or_else(|| PluginError::Schema {
                        message: "missing `id`".to_owned(),
                    })?,
                )?;
                let view_id = view(&params)?;
                let args = params.get("args").cloned().unwrap_or(Value::Null);
                json(self.commands.dispatch(&id, view_id, args)?)
            }
            other => Err(unsupported_method("editor.commands", other)),
        }
    }

    fn syntax_call(
        &self,
        plugin: &PluginId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        self.authorize(plugin, "editor.syntax.read")?;
        match method {
            "scopes" => json(self.syntax.scopes(view(&params)?)?),
            "folds" => json(self.syntax.folds(view(&params)?)?),
            "symbols" => json(self.syntax.symbols(view(&params)?)?),
            "grammar" => {
                let language = params.get("language").and_then(Value::as_str).ok_or_else(|| {
                    PluginError::Schema { message: "missing `language`".to_owned() }
                })?;
                json(self.syntax.grammar_available(language))
            }
            other => Err(unsupported_method("editor.syntax", other)),
        }
    }

    fn lsp_call(
        &self,
        plugin: &PluginId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        self.authorize(plugin, "editor.lsp.read")?;
        let request = from_value::<LspRequest>(params)?;
        match method {
            "hover" => self.lsp.hover(&request),
            "completion" => self.lsp.completion(&request),
            "definition" => self.lsp.definition(&request),
            "references" => self.lsp.references(&request),
            "formatting" => self.lsp.formatting(&request),
            "code_actions" => self.lsp.code_actions(&request),
            "rename_preview" => self.lsp.rename_preview(&request),
            other => Err(unsupported_method("editor.lsp", other)),
        }
    }

    fn decorations_call(
        &self,
        plugin: &PluginId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        match method {
            "collect" => {
                self.authorize(plugin, "editor.decorations.write")?;
                let rev = params.get("rev").and_then(Value::as_u64).unwrap_or(0);
                json(self.decorations.collect(view(&params)?, rev)?)
            }
            "apply" => {
                self.authorize(plugin, "editor.decorations.write")?;
                let patch = from_value(params)?;
                self.decorations.apply(Some(plugin), &patch)?;
                Ok(Value::Null)
            }
            other => Err(unsupported_method("editor.decorations", other)),
        }
    }

    fn ui_call(
        &self,
        plugin: &PluginId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        self.authorize(plugin, "editor.ui.write")?;
        match method {
            "render" => {
                let request = from_value::<UiRenderRequest>(params)?;
                json(self.ui.render(&request.id, request.view_id, request.revision)?)
            }
            "panel" => {
                let id = from_value(params.get("id").cloned().unwrap_or(Value::Null))?;
                json(self.ui.panel(&id))
            }
            "overlay" => {
                let id = from_value(params.get("id").cloned().unwrap_or(Value::Null))?;
                json(self.ui.overlay(&id))
            }
            "picker" => {
                let id = from_value(params.get("id").cloned().unwrap_or(Value::Null))?;
                json(self.ui.picker(&id))
            }
            other => Err(unsupported_method("editor.ui", other)),
        }
    }

    fn tasks_call(
        &self,
        plugin: &PluginId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        self.authorize(plugin, "editor.tasks.run")?;
        match method {
            "run" => {
                let kind = params
                    .get("kind")
                    .and_then(Value::as_str)
                    .ok_or_else(|| PluginError::Schema { message: "missing `kind`".to_owned() })?;
                let timeout_ms = params.get("timeout_ms").and_then(Value::as_u64).unwrap_or(1_000);
                let payload = params.get("payload").cloned().unwrap_or(Value::Null);
                json(self.tasks.run_kind(
                    kind,
                    &payload,
                    std::time::Duration::from_millis(timeout_ms.max(1)),
                )?)
            }
            other => Err(unsupported_method("editor.tasks", other)),
        }
    }

    fn fs_call(
        &self,
        plugin: &PluginId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        match method {
            "read" => {
                self.authorize(plugin, "editor.fs.read")?;
                let path = string_param(&params, "path")?;
                let bytes = self.fs.read(path)?;
                Ok(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
            }
            "write" => {
                self.authorize(plugin, "editor.fs.write")?;
                let path = string_param(&params, "path")?;
                let text = string_param(&params, "text")?;
                self.fs.write(path, text.as_bytes())?;
                Ok(Value::Null)
            }
            "stat" => {
                self.authorize(plugin, "editor.fs.read")?;
                json(self.fs.stat(string_param(&params, "path")?)?)
            }
            "list" => {
                self.authorize(plugin, "editor.fs.read")?;
                json(self.fs.list(string_param(&params, "path").unwrap_or("."))?)
            }
            other => Err(unsupported_method("editor.fs", other)),
        }
    }

    fn config_call(
        &self,
        plugin: &PluginId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        self.authorize(plugin, "editor.config.read")?;
        match method {
            "host" => Ok(self.config.host_config()),
            "section" => json(self.config.section(string_param(&params, "name")?)?),
            "plugin" => Ok(self.config.plugin_config(string_param(&params, "plugin")?)),
            other => Err(unsupported_method("editor.config", other)),
        }
    }
}

/// Helper used by tests and hosts building service scopes.
pub fn default_fs_scope(read: bool, write: bool) -> FsScope {
    FsScope { read, write, allow: Vec::new() }
}

fn json<T: serde::Serialize>(value: T) -> Result<Value, PluginError> {
    serde_json::to_value(value).map_err(|error| PluginError::Schema { message: error.to_string() })
}

fn from_value<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, PluginError> {
    serde_json::from_value(value)
        .map_err(|error| PluginError::Schema { message: error.to_string() })
}

fn buffer(params: &Value) -> Result<BufferId, PluginError> {
    let value = params
        .get("buffer_id")
        .cloned()
        .ok_or_else(|| PluginError::Schema { message: "missing `buffer_id`".to_owned() })?;
    from_value(value)
}

fn view(params: &Value) -> Result<ViewId, PluginError> {
    let value = params
        .get("view_id")
        .cloned()
        .ok_or_else(|| PluginError::Schema { message: "missing `view_id`".to_owned() })?;
    from_value(value)
}

fn usize_param(params: &Value, key: &str) -> Result<usize, PluginError> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .ok_or_else(|| PluginError::Schema { message: format!("missing or invalid `{key}`") })
}

fn string_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, PluginError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::Schema { message: format!("missing or invalid `{key}`") })
}

fn unsupported_method(service: &'static str, method: &str) -> PluginError {
    let _ = (service, method);
    PluginError::Unsupported { feature: "service method" }
}
