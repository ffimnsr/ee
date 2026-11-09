//! `impl App` methods: declarative plugin UI integration.
//!
//! The frontend only invokes commands by id and routes bounded interactions. It
//! never executes plugin callbacks or reads plugin manifests.

use serde_json::{Value, json};

use super::*;
use crate::plugin_ui::PluginUiEventKind;

impl App {
    /// Invoke a plugin command by id. Unknown ids surface a status error.
    pub(crate) fn invoke_plugin_command(&mut self, id: &str, args: Value) {
        if !self.backend.plugin_ui.has_command(id) {
            self.backend.status_message = Some(format!("unknown plugin command: {id}"));
            return;
        }
        if let Err(error) = self.backend.send_plugin_command(id, args) {
            self.backend.status_message = Some(format!("plugin command failed: {error}"));
        }
    }

    /// Command palette rows for contributed commands, including category and
    /// ex-command usage metadata.
    #[must_use]
    pub(crate) fn plugin_command_help_items(&self) -> Vec<String> {
        let mut rows = self
            .backend
            .plugin_ui
            .commands()
            .map(|command| {
                let ex = self
                    .backend
                    .plugin_ui
                    .registry
                    .ex_commands()
                    .find(|ex| ex.command == command.id);
                let alias = ex.map(|ex| ex.name.as_str()).unwrap_or_else(|| command.id.as_str());
                let usage = ex.and_then(|ex| ex.usage.as_deref()).unwrap_or("");
                let category = command.category.as_deref().unwrap_or("plugin");
                let mut row = format!("[{category}] :{alias}");
                if !usage.is_empty() {
                    row.push(' ');
                    row.push_str(usage);
                }
                row.push(' ');
                row.push_str(command.title.trim());
                if !command.description.trim().is_empty() {
                    row.push_str(" — ");
                    row.push_str(command.description.trim());
                }
                row
            })
            .collect::<Vec<_>>();
        rows.sort();
        rows
    }

    /// Try to execute a contributed ex command. Returns `true` when the head
    /// matched a plugin alias, including usage-error handling.
    pub(crate) fn try_execute_plugin_ex_command(&mut self, command: &str) -> bool {
        let trimmed = command.trim_start();
        if trimmed.is_empty() {
            return false;
        }
        let split_at = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        let head = &trimmed[..split_at];
        let tail = trimmed[split_at..].trim_start();
        let Some(command_id) = self.backend.plugin_ui.resolve_ex_alias(head).cloned() else {
            return false;
        };

        let ex = self
            .backend
            .plugin_ui
            .registry
            .ex_commands()
            .find(|ex| ex.name == head || ex.aliases.iter().any(|alias| alias == head));
        let usage = ex.and_then(|ex| ex.usage.clone());
        if tail.is_empty() && self.plugin_command_requires_args(command_id.as_str()) {
            let usage = usage.unwrap_or_else(|| String::from("<args>"));
            self.backend.status_message = Some(format!(":{head}: usage: {usage}"));
            return true;
        }

        let args = parse_plugin_ex_args(tail);
        if let Err(error) = self.backend.send_plugin_command(command_id.as_str(), args) {
            self.backend.status_message = Some(format!(":{head}: {error}"));
        }
        true
    }

    fn plugin_command_requires_args(&self, id: &str) -> bool {
        self.backend
            .plugin_ui
            .registry
            .command(id)
            .and_then(|command| command.args_schema.as_ref())
            .and_then(|schema| schema.get("required"))
            .and_then(Value::as_array)
            .is_some_and(|required| !required.is_empty())
    }

    /// Plugin ex-command aliases for command-line completion.
    #[must_use]
    pub(crate) fn plugin_ex_command_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for ex in self.backend.plugin_ui.registry.ex_commands() {
            names.push(ex.name.clone());
            names.extend(ex.aliases.iter().cloned());
        }
        names.sort();
        names.dedup();
        names
    }

    /// Close the active plugin picker and notify its owner.
    pub(crate) fn close_plugin_picker(&mut self) {
        if let Some(id) = self.backend.plugin_ui.close_picker() {
            let _ = self.backend.send_plugin_ui_event(
                id.as_str(),
                PluginUiEventKind::Close,
                Value::Null,
            );
        }
    }

    /// Confirm the selected plugin picker row, routing activation back.
    pub(crate) fn confirm_plugin_picker(&mut self) {
        let Some(picker) = self.backend.plugin_ui.picker.as_ref() else { return };
        let Some(row) = picker.selected_item() else { return };
        let id = picker.id.clone();
        let payload = crate::plugin_ui::picker::row_event_payload(row);
        if let Err(error) =
            self.backend.send_plugin_ui_event(id.as_str(), PluginUiEventKind::Activate, payload)
        {
            self.backend.status_message = Some(format!("ui event failed: {error}"));
        }
    }

    /// Route local filter changes back to the owning contribution.
    pub(crate) fn send_plugin_picker_filter(&mut self) {
        let Some(picker) = self.backend.plugin_ui.picker.as_ref() else { return };
        let id = picker.id.clone();
        let query = Value::String(picker.query.clone());
        let _ = self.backend.send_plugin_ui_event(id.as_str(), PluginUiEventKind::Filter, query);
    }

    /// Route the current selection back to the owning contribution.
    pub(crate) fn send_plugin_picker_selection(&mut self) {
        let Some(picker) = self.backend.plugin_ui.picker.as_ref() else { return };
        let id = picker.id.clone();
        let payload = picker
            .selected_item()
            .map(crate::plugin_ui::picker::row_event_payload)
            .unwrap_or(Value::Null);
        let _ = self.backend.send_plugin_ui_event(id.as_str(), PluginUiEventKind::Select, payload);
    }

    /// Text input while a plugin picker is focused.
    pub(crate) fn handle_plugin_picker_text_input(&mut self, key: KeyEvent) {
        if let KeyCode::Char(ch) = key.code
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && let Some(picker) = self.backend.plugin_ui.picker.as_mut()
        {
            picker.push_char(ch);
        }
        self.send_plugin_picker_filter();
    }

    /// Whether a plugin picker currently owns picker focus.
    #[must_use]
    pub(crate) fn plugin_picker_active(&self) -> bool {
        self.backend.plugin_ui.picker.is_some()
    }
}

fn parse_plugin_ex_args(tail: &str) -> Value {
    if tail.is_empty() {
        return json!({});
    }
    serde_json::from_str::<Value>(tail).unwrap_or_else(|_| json!({ "args": tail }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ex_arguments_parse_json_and_fall_back_to_text() {
        assert_eq!(parse_plugin_ex_args(""), json!({}));
        assert_eq!(parse_plugin_ex_args("{\"x\":1}"), json!({"x": 1}));
        assert_eq!(parse_plugin_ex_args("hello"), json!({"args": "hello"}));
    }
}
