//! `impl BufferManager` methods: buffers.
use super::*;

impl BufferManager {
    pub(crate) fn open_buffer(&mut self, path: Option<PathBuf>) -> io::Result<BufferId> {
        let rpc_id = self.next_rpc_id;
        self.next_rpc_id += 1;

        if let Err(error) = crate::config::configure_runtime_loader_for_file(path.as_deref(), true)
        {
            eprintln!("ee: warning: failed to configure runtime languages: {error}");
        }
        send_lsp_config_notification(&self.tx, path.as_deref())?;

        // Register a one-shot channel so the reader thread can hand us the
        // view_id response without blocking the reader loop.
        let (resp_tx, resp_rx) = std_mpsc::sync_channel::<Value>(1);
        {
            let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(rpc_id, resp_tx);
        }

        send_rpc_request(
            &self.tx,
            rpc_id,
            "new_view",
            json!({ "file_path": path.as_ref().map(|p| p.to_string_lossy().to_string()) }),
        )?;

        let response = resp_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "new_view timed out"))?;
        let view_id = parse_response(response)?
            .as_str()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "new_view returned non-string id")
            })?
            .to_owned();

        let buf_id = self.next_buf_id;
        self.next_buf_id += 1;
        let idx = self.bufs.len();

        self.view_to_idx.insert(view_id.clone(), idx);
        let mtime =
            path.as_ref().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok());
        self.bufs.push(BufState {
            id: buf_id,
            path,
            display_name: None,
            view_id,
            editor_config_synced: false,
            pending_line_request: false,
            line_cache: Vec::new(),
            lines: Vec::new(),
            cursor_line: 0,
            cursor_col: 0,
            pristine: true,
            save_complete: true,
            last_save_generation: 0,
            completed_save_generation: 0,
            last_save_result_generation: 0,
            last_save_succeeded: true,
            last_save_permission_denied: false,
            last_save_error_message: None,
            status_message: None,
            last_scroll: None,
            mtime,
            externally_modified: false,
            diagnostics: Vec::new(),
            annotations: Vec::new(),
            is_vlf: false,
            vlf_cache_start_line: 0,
            vlf_previous_viewport: None,
            vlf_generation: 0,
            vlf_approx_line_count: 0,
            vlf_line_count_exact: false,
            pending_vlf_tail_jump: false,
            vlf_search_ranges: Vec::new(),
        });
        Ok(buf_id)
    }

    pub(crate) fn open_named_scratch_buffer(
        &mut self,
        title: impl Into<String>,
    ) -> io::Result<BufferId> {
        let buf_id = self.open_buffer(None)?;
        if let Some(buf) = self.bufs.iter_mut().find(|buf| buf.id == buf_id) {
            buf.display_name = Some(title.into());
        }
        Ok(buf_id)
    }

    /// Close a buffer.  Fails if it would leave no open buffers.
    pub(crate) fn close_buffer(&mut self, id: BufferId) -> io::Result<()> {
        if self.bufs.len() <= 1 {
            return Err(io::Error::other("cannot close last buffer"));
        }
        let pos = self
            .bufs
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;

        let view_id = self.bufs[pos].view_id.clone();
        self.vlf_viewports.cancel_view(&view_id);
        let _ = send_xi_notification(&self.tx, "close_view", json!({ "view_id": view_id }));

        self.bufs.remove(pos);

        // Rebuild index map since positions shifted.
        self.view_to_idx.clear();
        for (i, b) in self.bufs.iter().enumerate() {
            self.view_to_idx.insert(b.view_id.clone(), i);
        }

        // Adjust current.
        if self.bufs.is_empty() {
            self.current = 0; // unreachable, guarded above
        } else if self.current >= self.bufs.len() {
            self.current = self.bufs.len() - 1;
        } else if self.current > pos {
            self.current -= 1;
        } else if self.current == pos {
            self.current = pos.saturating_sub(1);
        }

        // Adjust alternate.
        if let Some(alt) = self.alternate {
            if alt == pos {
                self.alternate = None;
            } else if alt > pos {
                self.alternate = Some(alt - 1);
            }
        }
        self.access_history.retain(|candidate| *candidate != id);
        self.modified_history.retain(|candidate| *candidate != id);
        Ok(())
    }

    /// Switch to buffer at list index `idx`, saving current as alternate.
    pub(crate) fn switch_to_idx(&mut self, idx: usize) {
        if idx < self.bufs.len() && idx != self.current {
            self.access_history.push(self.bufs[self.current].id);
            self.alternate = Some(self.current);
            self.current = idx;
        }
    }

    /// Switch to buffer by [`BufferId`].
    pub(crate) fn switch_to_id(&mut self, id: BufferId) -> io::Result<()> {
        let idx = self
            .bufs
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        self.switch_to_idx(idx);
        Ok(())
    }

    /// Switch to the alternate buffer (`:b#` / Ctrl-^).
    pub(crate) fn switch_alternate(&mut self) -> io::Result<()> {
        let alt = self.alternate.ok_or_else(|| io::Error::other("no alternate buffer"))?;
        self.switch_to_idx(alt);
        Ok(())
    }

    pub(crate) fn switch_last_accessed(&mut self) -> io::Result<()> {
        while let Some(id) = self.access_history.pop() {
            let Some(idx) = self.bufs.iter().position(|buf| buf.id == id) else {
                continue;
            };
            if idx == self.current {
                continue;
            }
            self.switch_to_idx(idx);
            return Ok(());
        }
        Err(io::Error::other("no last accessed buffer"))
    }

    pub(crate) fn switch_last_modified(&mut self) -> io::Result<()> {
        let current_id = self.bufs[self.current].id;
        let Some(id) =
            self.modified_history.iter().copied().find(|candidate| *candidate != current_id)
        else {
            return Err(io::Error::other("no last modified buffer"));
        };
        self.switch_to_id(id)
    }

    pub(crate) fn note_buffer_modified(&mut self, id: BufferId) {
        if self.bufs.iter().all(|buf| buf.id != id) {
            return;
        }
        self.modified_history.retain(|candidate| *candidate != id);
        self.modified_history.insert(0, id);
    }

    pub(crate) fn restore_cursor(
        &mut self,
        id: BufferId,
        line: usize,
        col: usize,
    ) -> io::Result<()> {
        let idx = self
            .bufs
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        let view_id = self.bufs[idx].view_id.clone();
        self.bufs[idx].cursor_line = line;
        self.bufs[idx].cursor_col = col;
        send_xi_notification(
            &self.tx,
            "edit",
            json!({
                "view_id": view_id,
                "method": "gesture",
                "params": {
                    "line": line as u64,
                    "col": col as u64,
                    "ty": "point_select",
                },
            }),
        )
    }

    /// Cycle to the next buffer (wrapping).
    pub(crate) fn next_buffer(&mut self) {
        if self.bufs.len() > 1 {
            let next = (self.current + 1) % self.bufs.len();
            self.switch_to_idx(next);
        }
    }

    /// Cycle to the previous buffer (wrapping).
    pub(crate) fn prev_buffer(&mut self) {
        if self.bufs.len() > 1 {
            let prev = if self.current == 0 { self.bufs.len() - 1 } else { self.current - 1 };
            self.switch_to_idx(prev);
        }
    }

    /// Build a `:ls`-style buffer list string.
    pub(crate) fn list_buffers_str(&self) -> String {
        self.bufs
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let flag = if i == self.current {
                    "%"
                } else if self.alternate == Some(i) {
                    "#"
                } else {
                    " "
                };
                let modified = if b.pristine { " " } else { "+" };
                format!("{flag}{modified} {} {}", b.id, b.title())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
