//! `impl BufferManager` methods: edits.
use super::*;

impl BufferManager {
    pub(crate) fn send_edit(&self, method: &str, params: Value) -> io::Result<()> {
        let view_id = &self.bufs[self.current].view_id;
        send_xi_notification(
            &self.tx,
            "edit",
            json!({
                "view_id": view_id,
                "method": method,
                "params": params,
            }),
        )
    }

    pub(super) fn send_request(&mut self, method: &str, params: Value) -> io::Result<Value> {
        let rpc_id = self.next_rpc_id;
        self.next_rpc_id = self.next_rpc_id.saturating_add(1);

        let (resp_tx, resp_rx) = std_mpsc::sync_channel::<Value>(1);
        {
            let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(rpc_id, resp_tx);
        }

        send_rpc_request(&self.tx, rpc_id, method, params)?;

        let timeout = if cfg!(test) { Duration::from_secs(15) } else { Duration::from_secs(5) };
        let response = resp_rx
            .recv_timeout(timeout)
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, format!("{method} timed out")))?;
        parse_response(response)
    }

    pub(crate) fn save(&mut self) -> io::Result<()> {
        let id = self.bufs[self.current].id;
        self.save_buffer(id)
    }

    pub(crate) fn prepare_elevated_save_draft(&mut self) -> io::Result<(PathBuf, PathBuf, Value)> {
        let view_id = self.active().view_id.clone();
        let response =
            self.send_request("prepare_elevated_save_draft", json!({ "view_id": view_id }))?;
        let draft_path = response
            .get("draft_path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing draft_path"))?;
        let file_path = response
            .get("file_path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing file_path"))?;
        let saved_rev_id = response
            .get("saved_rev_id")
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing saved_rev_id"))?;
        Ok((draft_path, file_path, saved_rev_id))
    }

    pub(crate) fn finalize_elevated_save(
        &mut self,
        path: &std::path::Path,
        saved_rev_id: Value,
    ) -> io::Result<()> {
        let view_id = self.active().view_id.clone();
        let _ = self.send_request(
            "finalize_elevated_save",
            json!({
                "view_id": view_id,
                "file_path": path.to_string_lossy().to_string(),
                "saved_rev_id": saved_rev_id,
            }),
        )?;
        Ok(())
    }

    pub(crate) fn save_buffer(&mut self, id: BufferId) -> io::Result<()> {
        let idx = self
            .bufs
            .iter()
            .position(|buf| buf.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        self.flush_view_edits(idx)?;
        let buf = &self.bufs[idx];
        let Some(path) = buf.path.clone() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "scratch buffer has no path"));
        };
        let view_id = buf.view_id.clone();
        send_xi_notification(
            &self.tx,
            "save",
            json!({
                "view_id": view_id,
                "file_path": path.to_string_lossy().to_string(),
            }),
        )?;
        let baseline_generation = self.bufs[idx].last_save_generation;
        self.bufs[idx].save_complete = false;
        self.bufs[idx].last_save_error_message = None;
        self.wait_for_buffer_save(id, &path, baseline_generation)?;
        let display = path.display().to_string();
        // Refresh mtime after the save so external-change detection stays accurate.
        let new_mtime = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        let buf = &mut self.bufs[idx];
        buf.mtime = new_mtime;
        buf.externally_modified = false;
        buf.status_message = Some(format!("saved {display}"));
        // Remove crash-recovery artifact now that the file is persisted.
        if let Some(rp) = recovery_file_path(&path) {
            let _ = std::fs::remove_file(rp);
        }
        Ok(())
    }

    pub(crate) fn flush_all_pending_edits(&mut self) -> io::Result<()> {
        for idx in 0..self.bufs.len() {
            self.flush_view_edits(idx)?;
        }
        Ok(())
    }

    /// Query core-owned Tree-sitter dependency facts for one open buffer.
    #[cfg(feature = "agents")]
    pub(crate) fn symbol_dependency_map(
        &mut self,
        id: BufferId,
        path: String,
        line: u32,
        character: u32,
        language_id: String,
    ) -> io::Result<Value> {
        let view_id = self
            .bufs
            .iter()
            .find(|buf| buf.id == id)
            .map(|buf| buf.view_id.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        self.send_request(
            "symbol_dependency_map",
            json!({
                "view_id": view_id,
                "path": path,
                "line": line,
                "character": character,
                "language_id": language_id,
            }),
        )
    }

    pub(crate) fn buffer_pristine(&mut self, id: BufferId) -> io::Result<bool> {
        let view_id = self
            .bufs
            .iter()
            .find(|buf| buf.id == id)
            .map(|buf| buf.view_id.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        let response = self.send_request("buffer_pristine", json!({ "view_id": view_id }))?;
        response.as_bool().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "buffer_pristine returned non-bool")
        })
    }

    pub(super) fn poll_buffer_save_status(&mut self, id: BufferId) -> io::Result<(u64, bool)> {
        let view_id = self
            .bufs
            .iter()
            .find(|buf| buf.id == id)
            .map(|buf| buf.view_id.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        let response = self.send_request("save_status", json!({ "view_id": view_id }))?;
        let generation = response.get("generation").and_then(Value::as_u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "save_status missing generation")
        })?;
        let complete = response.get("complete").and_then(Value::as_bool).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "save_status missing complete")
        })?;
        Ok((generation, complete))
    }

    pub(super) fn wait_for_buffer_save(
        &mut self,
        id: BufferId,
        path: &std::path::Path,
        baseline_generation: u64,
    ) -> io::Result<()> {
        let timeout = if cfg!(test) { Duration::from_secs(15) } else { Duration::from_secs(5) };
        let deadline = Instant::now() + timeout;
        let mut target_generation = None;
        loop {
            self.sync_pending_events_for_whole_document()?;

            let idx = self
                .bufs
                .iter()
                .position(|buf| buf.id == id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
            if target_generation.is_none()
                && self.bufs[idx].last_save_generation > baseline_generation
            {
                target_generation = Some(self.bufs[idx].last_save_generation);
            }
            let (status_generation, status_complete) = self.poll_buffer_save_status(id)?;
            if target_generation.is_none() && status_generation > baseline_generation {
                target_generation = Some(status_generation);
            }
            if status_complete
                && target_generation.is_some_and(|generation| status_generation >= generation)
            {
                let idx =
                    self.bufs.iter().position(|buf| buf.id == id).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "buffer not found")
                    })?;
                self.bufs[idx].last_save_generation =
                    self.bufs[idx].last_save_generation.max(status_generation);
                self.bufs[idx].completed_save_generation =
                    self.bufs[idx].completed_save_generation.max(status_generation);
                self.bufs[idx].save_complete = true;
                if self.bufs[idx].last_save_result_generation >= status_generation {
                    if self.bufs[idx].last_save_succeeded {
                        return Ok(());
                    }
                    let kind = if self.bufs[idx].last_save_permission_denied {
                        io::ErrorKind::PermissionDenied
                    } else {
                        io::ErrorKind::Other
                    };
                    let message = self.bufs[idx]
                        .last_save_error_message
                        .clone()
                        .unwrap_or_else(|| format!("save failed: {}", path.display()));
                    return Err(io::Error::new(kind, message));
                }
            }
            if target_generation.is_some_and(|generation| {
                self.bufs[idx].completed_save_generation >= generation
                    && self.bufs[idx].last_save_result_generation >= generation
            }) {
                self.bufs[idx].save_complete = true;
                if self.bufs[idx].last_save_succeeded {
                    return Ok(());
                }
                let kind = if self.bufs[idx].last_save_permission_denied {
                    io::ErrorKind::PermissionDenied
                } else {
                    io::ErrorKind::Other
                };
                let message = self.bufs[idx]
                    .last_save_error_message
                    .clone()
                    .unwrap_or_else(|| format!("save failed: {}", path.display()));
                return Err(io::Error::new(kind, message));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("save timed out: {}", path.display()),
                ));
            }

            thread::sleep(Duration::from_millis(10));
        }
    }
    pub(crate) fn delete_line_range(
        &mut self,
        start_line: usize,
        end_line: usize,
    ) -> io::Result<()> {
        self.send_edit(
            "delete_line_range",
            json!({
                "start_line": start_line,
                "end_line": end_line,
            }),
        )
    }

    pub(crate) fn goto_column(
        &mut self,
        display_col: usize,
        modify_selection: bool,
    ) -> io::Result<()> {
        self.send_edit(
            "goto_column",
            json!({
                "display_col": display_col,
                "modify_selection": modify_selection,
            }),
        )
    }

    pub(crate) fn add_newline_above(&mut self) -> io::Result<()> {
        self.send_edit("add_newline_above", json!({}))
    }

    pub(crate) fn add_newline_below(&mut self) -> io::Result<()> {
        self.send_edit("add_newline_below", json!({}))
    }

    pub(crate) fn join_selections(&mut self, select_space: bool) -> io::Result<()> {
        self.send_edit("join_selections", json!({ "select_space": select_space }))
    }

    pub(crate) fn extend_line_below(&mut self, count: usize) -> io::Result<()> {
        self.send_edit("extend_line_below", json!({ "count": count }))
    }

    pub(crate) fn extend_to_line_bounds(&mut self) -> io::Result<()> {
        self.send_edit("extend_to_line_bounds", json!({}))
    }

    pub(crate) fn shrink_to_line_bounds(&mut self) -> io::Result<()> {
        self.send_edit("shrink_to_line_bounds", json!({}))
    }

    pub(crate) fn move_word_start(
        &mut self,
        forward: bool,
        long_word: bool,
        modify_selection: bool,
    ) -> io::Result<()> {
        self.send_edit(
            "move_word_start",
            json!({
                "forward": forward,
                "long_word": long_word,
                "modify_selection": modify_selection,
            }),
        )
    }

    pub(crate) fn move_word_end(
        &mut self,
        long_word: bool,
        modify_selection: bool,
    ) -> io::Result<()> {
        self.send_edit(
            "move_word_end",
            json!({
                "long_word": long_word,
                "modify_selection": modify_selection,
            }),
        )
    }

    pub(crate) fn find_char(
        &mut self,
        target: char,
        forward: bool,
        inclusive: bool,
        modify_selection: bool,
    ) -> io::Result<()> {
        self.send_edit(
            "find_char",
            json!({
                "target": target,
                "forward": forward,
                "inclusive": inclusive,
                "modify_selection": modify_selection,
            }),
        )
    }

    pub(crate) fn move_to_matching_bracket(&mut self, modify_selection: bool) -> io::Result<()> {
        self.send_edit(
            "move_to_matching_bracket",
            json!({
                "modify_selection": modify_selection,
            }),
        )
    }

    pub(crate) fn delete_block(
        &mut self,
        start_line: usize,
        end_line: usize,
        left_col: usize,
        right_col: usize,
    ) -> io::Result<()> {
        self.send_edit(
            "delete_block",
            json!({
                "start_line": start_line,
                "end_line": end_line,
                "left_col": left_col,
                "right_col": right_col,
            }),
        )
    }

    pub(crate) fn replay_block_insert(
        &mut self,
        start_line: usize,
        end_line: usize,
        column: usize,
        text: &str,
        append: bool,
    ) -> io::Result<()> {
        self.send_edit(
            "replay_block_insert",
            json!({
                "start_line": start_line,
                "end_line": end_line,
                "column": column,
                "text": text,
                "append": append,
            }),
        )
    }
}
