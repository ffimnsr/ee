//! `impl BufferManager` methods: pump.
use super::*;

impl BufferManager {
    pub(crate) fn sync_pending_events(&mut self) -> io::Result<()> {
        self.sync_pending_events_with_scope(LineRequestScope::Viewport)
    }

    pub(crate) fn sync_pending_events_for_whole_document(&mut self) -> io::Result<()> {
        self.sync_pending_events_with_scope(LineRequestScope::WholeDocument)
    }

    pub(super) fn sync_pending_events_with_scope(
        &mut self,
        scope: LineRequestScope,
    ) -> io::Result<()> {
        let mut idle_rounds = 0;
        for _ in 0..24 {
            self.request_all_invalid_lines(scope)?;
            match recv_with_timeout(&mut self.backend_rx, Duration::from_millis(10)) {
                Some(event) => {
                    idle_rounds = 0;
                    self.apply_event_to_buffer(event)?;
                    while let Ok(event) = self.backend_rx.try_recv() {
                        self.apply_event_to_buffer(event)?;
                    }
                }
                None if self.has_pending_line_work(scope) => continue,
                None => {
                    idle_rounds += 1;
                    if idle_rounds >= Self::SYNC_IDLE_LIMIT {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn pump(&mut self) -> io::Result<()> {
        self.sync_pending_events()
    }

    /// Repeatedly drain pending events until `predicate` holds for the active
    /// buffer, or the 2-second safety deadline expires.
    ///
    /// Use this instead of bare `pump()` when a test must wait for xi-core to
    /// finish processing a batch of keystrokes before inspecting state.
    #[cfg(test)]
    pub(crate) fn pump_until<F>(&mut self, predicate: F) -> io::Result<()>
    where
        F: Fn(&BufState) -> bool,
    {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            self.sync_pending_events()?;
            if predicate(self.active()) || Instant::now() >= deadline {
                break;
            }
        }
        Ok(())
    }

    /// Test-only constructor that builds a minimal `BufferManager` around
    /// pre-existing channel ends and a known view_id.
    #[cfg(test)]
    pub(crate) fn pending_requests_for_test(&self) -> PendingRequests {
        Arc::clone(&self.pending)
    }

    #[cfg(test)]
    pub(crate) fn test_new(
        tx: std_mpsc::Sender<String>,
        backend_rx: std_mpsc::Receiver<BackendEvent>,
        view_id: String,
    ) -> Self {
        let (internal_tx, mut internal_rx) = mpsc::channel::<String>(64);
        thread::spawn(move || {
            while let Some(message) = internal_rx.blocking_recv() {
                if tx.send(message).is_err() {
                    break;
                }
            }
        });

        let buf = BufState {
            id: 1,
            path: None,
            display_name: None,
            view_id: view_id.clone(),
            editor_config_synced: true,
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
            mtime: None,
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
        };
        let mut view_to_idx = HashMap::new();
        view_to_idx.insert(view_id, 0);
        Self {
            tx: internal_tx,
            backend_rx,
            core_thread: None,
            reader_thread: None,
            reader_shutdown: Arc::new(AtomicBool::new(false)),
            bufs: vec![buf],
            view_to_idx,
            current: 0,
            alternate: None,
            access_history: Vec::new(),
            modified_history: Vec::new(),
            next_buf_id: 2,
            next_rpc_id: 2,
            pending: Arc::new(Mutex::new(HashMap::new())),
            pending_locations: Vec::new(),
            pending_symbols: Vec::new(),
            pending_agent_tool_results: Vec::new(),
            available_plugins_by_view: HashMap::new(),
            pending_ui_actions: Vec::new(),
            startup_profile: StartupProfile::default(),
            startup_profile_active: false,
            vlf_viewports: VlfViewportScheduler::default(),
        }
    }

    // ── External change detection ─────────────────────────────────────────

    /// Drain accumulated location results for App-level dispatch.
    pub(crate) fn drain_pending_locations(
        &mut self,
    ) -> Vec<(String, String, Vec<NavigationTarget>)> {
        std::mem::take(&mut self.pending_locations)
    }

    pub(crate) fn drain_pending_symbols(&mut self) -> Vec<(String, String, Vec<SymbolItem>)> {
        std::mem::take(&mut self.pending_symbols)
    }

    #[cfg(feature = "agents")]
    pub(crate) fn drain_pending_agent_tool_results(&mut self) -> Vec<(String, String, Value)> {
        std::mem::take(&mut self.pending_agent_tool_results)
    }

    pub(crate) fn available_plugins_for_current_view(&self) -> &[ClientPluginInfo] {
        self.available_plugins_by_view
            .get(&self.bufs[self.current].view_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub(crate) fn drain_pending_ui_actions(&mut self) -> Vec<PendingUiAction> {
        std::mem::take(&mut self.pending_ui_actions)
    }

    /// Check all buffers for filesystem changes since last open/save.
    /// Sets `BufState::externally_modified` when a newer mtime is detected.
    pub(crate) fn check_external_changes(&mut self) {
        for buf in &mut self.bufs {
            let Some(path) = &buf.path else { continue };
            if buf.externally_modified {
                continue; // already notified
            }
            let current_mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
            if let (Some(stored), Some(current)) = (buf.mtime, current_mtime)
                && current > stored
            {
                buf.externally_modified = true;
            }
        }
    }

    /// Reload the buffer identified by `id` from its backing file, discarding
    /// local edits.  Closes the current xi view and opens a fresh one.
    pub(crate) fn reload_buffer(&mut self, id: BufferId) -> io::Result<()> {
        let idx = self
            .bufs
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        let path = self.bufs[idx].path.clone();
        let old_view_id = self.bufs[idx].view_id.clone();

        // Close the old xi view.
        self.vlf_viewports.cancel_view(&old_view_id);
        let _ = send_xi_notification(&self.tx, "close_view", json!({ "view_id": old_view_id }));
        self.view_to_idx.remove(&old_view_id);

        // Open a new xi view for the same path.
        send_lsp_config_notification(&self.tx, path.as_deref())?;
        let rpc_id = self.next_rpc_id;
        self.next_rpc_id += 1;

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
        let new_view_id = parse_response(response)?
            .as_str()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "new_view returned non-string id")
            })?
            .to_owned();

        let mtime =
            path.as_ref().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok());

        let buf = &mut self.bufs[idx];
        buf.view_id = new_view_id.clone();
        buf.display_name = None;
        buf.editor_config_synced = false;
        buf.line_cache = Vec::new();
        buf.lines = Vec::new();
        buf.cursor_line = 0;
        buf.cursor_col = 0;
        buf.pristine = true;
        buf.pending_line_request = false;
        buf.last_scroll = None;
        buf.is_vlf = false;
        buf.vlf_cache_start_line = 0;
        buf.vlf_generation = 0;
        buf.vlf_approx_line_count = 0;
        buf.vlf_line_count_exact = false;
        buf.pending_vlf_tail_jump = false;
        buf.vlf_search_ranges.clear();
        buf.status_message = Some("reloaded".to_owned());
        buf.mtime = mtime;
        buf.externally_modified = false;

        self.view_to_idx.insert(new_view_id, idx);
        Ok(())
    }
}
