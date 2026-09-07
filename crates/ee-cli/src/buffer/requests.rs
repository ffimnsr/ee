//! `impl BufferManager` methods: requests.
use super::*;

impl BufferManager {
    pub(super) fn flush_view_edits(&mut self, idx: usize) -> io::Result<()> {
        let Some(view_id) = self.bufs.get(idx).map(|buf| buf.view_id.clone()) else {
            return Ok(());
        };
        let _ = self.send_request("selections_preview", json!({ "view_id": view_id }))?;
        self.sync_pending_events_for_whole_document()
    }

    pub(crate) fn reload_editor_config(&mut self) -> io::Result<()> {
        let (_, general_config, _) = crate::config::xi_config_tables_for_file(None);
        send_config_notification(&self.tx, json!("general"), general_config)?;
        send_lsp_config_notification(&self.tx, self.active().path.as_deref())?;
        for idx in 0..self.bufs.len() {
            self.sync_buffer_editor_config(idx)?;
        }
        Ok(())
    }

    pub(crate) fn request_completion(&mut self, index: Option<usize>) -> io::Result<()> {
        self.send_edit("request_completion", json!({ "index": index }))
    }

    pub(crate) fn request_definition(&mut self) -> io::Result<()> {
        self.send_edit("request_definition", json!({}))
    }

    pub(crate) fn request_declaration(&mut self) -> io::Result<()> {
        self.send_edit("request_declaration", json!({}))
    }

    pub(crate) fn request_type_definition(&mut self) -> io::Result<()> {
        self.send_edit("request_type_definition", json!({}))
    }

    pub(crate) fn request_references(&mut self) -> io::Result<()> {
        self.send_edit("request_references", json!({}))
    }

    pub(crate) fn request_implementation(&mut self) -> io::Result<()> {
        self.send_edit("request_implementation", json!({}))
    }

    pub(crate) fn request_document_symbols(&mut self) -> io::Result<()> {
        self.send_edit("request_document_symbols", json!({}))
    }

    pub(crate) fn request_workspace_symbols(&mut self, query: &str) -> io::Result<()> {
        self.send_edit("request_workspace_symbols", json!({ "query": query }))
    }

    pub(crate) fn format_document(&mut self) -> io::Result<()> {
        self.send_edit("format_document", json!({}))
    }

    pub(crate) fn request_code_actions(&mut self, index: Option<usize>) -> io::Result<()> {
        self.send_edit("request_code_actions", json!({ "index": index }))
    }

    pub(crate) fn request_rename(&mut self, new_name: &str) -> io::Result<()> {
        self.send_edit("request_rename", json!({ "new_name": new_name }))
    }

    pub(crate) fn stop_plugin(&mut self, plugin_name: &str) -> io::Result<()> {
        send_xi_notification(
            &self.tx,
            "plugin",
            json!({
                "command": "stop",
                "view_id": self.bufs[self.current].view_id,
                "plugin_name": plugin_name,
            }),
        )
    }

    pub(crate) fn restart_plugin(&mut self, plugin_name: &str) -> io::Result<()> {
        send_xi_notification(
            &self.tx,
            "plugin",
            json!({
                "command": "restart",
                "view_id": self.bufs[self.current].view_id,
                "plugin_name": plugin_name,
            }),
        )
    }
    pub(crate) fn substitute_preview(
        &mut self,
        start_line: usize,
        end_line: usize,
        pattern: &str,
        replacement: &str,
        global: bool,
        case_sensitive: bool,
    ) -> io::Result<Vec<LineReplacement>> {
        let view_id = self.bufs[self.current].view_id.clone();
        let response = self.send_request(
            "substitute_preview",
            json!({
                "view_id": view_id,
                "start_line": start_line,
                "end_line": end_line,
                "pattern": pattern,
                "replacement": replacement,
                "global": global,
                "case_sensitive": case_sensitive,
            }),
        )?;
        serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub(crate) fn filter_selections_preview(
        &mut self,
        pattern: &str,
        remove: bool,
    ) -> io::Result<Vec<SelectionRange>> {
        let view_id = self.bufs[self.current].view_id.clone();
        let response = self.send_request(
            "filter_selections_preview",
            json!({
                "view_id": view_id,
                "pattern": pattern,
                "remove": remove,
            }),
        )?;
        serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub(crate) fn selected_text_preview(&mut self, linewise: bool) -> io::Result<String> {
        let view_id = self.bufs[self.current].view_id.clone();
        let response = self.send_request(
            "selected_text_preview",
            json!({
                "view_id": view_id,
                "linewise": linewise,
            }),
        )?;
        serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub(crate) fn selections_preview(&mut self) -> io::Result<Vec<SelectionRange>> {
        let view_id = self.bufs[self.current].view_id.clone();
        let response = self.send_request("selections_preview", json!({ "view_id": view_id }))?;
        serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub(crate) fn block_text_preview(
        &mut self,
        start_line: usize,
        end_line: usize,
        left_col: usize,
        right_col: usize,
    ) -> io::Result<String> {
        let view_id = self.bufs[self.current].view_id.clone();
        let response = self.send_request(
            "block_text_preview",
            json!({
                "view_id": view_id,
                "start_line": start_line,
                "end_line": end_line,
                "left_col": left_col,
                "right_col": right_col,
            }),
        )?;
        serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub(crate) fn fold_ranges_preview(
        &mut self,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> io::Result<Vec<(usize, usize)>> {
        let view_id = self.bufs[self.current].view_id.clone();
        let response = self.send_request(
            "fold_ranges_preview",
            json!({
                "view_id": view_id,
                "start_line": start_line,
                "end_line": end_line,
            }),
        )?;
        let ranges: Vec<FoldRangePreview> = serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        Ok(ranges
            .into_iter()
            .filter(|range| range.body_end >= range.header_line)
            .map(|range| (range.header_line, range.body_end))
            .collect())
    }

    pub(crate) fn fold_range_at_cursor(&mut self) -> io::Result<Option<(usize, usize)>> {
        let line = self.active().cursor_line;
        Ok(self.fold_ranges_preview(Some(line), Some(line))?.into_iter().next())
    }

    pub(crate) fn select_chars_preview(&mut self, count: usize) -> io::Result<Vec<SelectionRange>> {
        let view_id = self.bufs[self.current].view_id.clone();
        let response = self.send_request(
            "select_chars_preview",
            json!({
                "view_id": view_id,
                "count": count,
            }),
        )?;
        serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub(crate) fn set_selections(&mut self, selections: &[SelectionRange]) -> io::Result<()> {
        self.send_edit("set_selections", json!({ "selections": selections }))
    }

    pub(crate) fn apply_line_replacements(
        &mut self,
        replacements: &[LineReplacement],
    ) -> io::Result<()> {
        self.send_edit("apply_line_replacements", json!({ "replacements": replacements }))
    }

    pub(crate) fn replace_line_range(
        &mut self,
        start_line: usize,
        end_line: usize,
        lines: &[String],
    ) -> io::Result<()> {
        self.send_edit(
            "replace_line_range",
            json!({
                "start_line": start_line,
                "end_line": end_line,
                "lines": lines,
            }),
        )
    }

    pub(crate) fn vlf_replace_range(
        &mut self,
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
        text: &str,
    ) -> io::Result<()> {
        self.send_edit(
            "vlf_replace_range",
            json!({
                "start_line": start_line,
                "start_col": start_col,
                "end_line": end_line,
                "end_col": end_col,
                "text": text,
            }),
        )
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn paste_register(&mut self, chars: &str, before: bool) -> io::Result<()> {
        self.send_edit("paste_register", json!({ "chars": chars, "before": before }))
    }

    pub(crate) fn request_hover(&mut self, position: Option<(usize, usize)>) -> io::Result<()> {
        let view_id = &self.bufs[self.current].view_id;
        let request_id = usize::try_from(self.next_rpc_id).unwrap_or(usize::MAX);
        self.next_rpc_id = self.next_rpc_id.saturating_add(1);
        let position = position.map(|(line, column)| {
            json!({
                "line": line,
                "column": column,
            })
        });
        send_xi_notification(
            &self.tx,
            "edit",
            json!({
                "view_id": view_id,
                "method": "request_hover",
                "params": {
                    "request_id": request_id,
                    "position": position,
                },
            }),
        )
    }
}
