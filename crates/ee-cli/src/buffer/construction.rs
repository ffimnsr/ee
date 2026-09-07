//! `impl BufferManager` methods: construction.
use super::*;

impl BufferManager {
    pub(super) const SYNC_IDLE_LIMIT: usize = 6;
    pub(super) const STARTUP_VLF_VIEWPORT_LINES: usize = 200;
    pub(super) const VLF_VIEWPORT_OVERSCAN_LINES: usize = 200;
    pub(super) const TAIL_VLF_PREFETCH_LINES: usize = 4096;

    pub(super) fn buffer_index_for_view(&self, view_id: &str) -> Option<usize> {
        self.view_to_idx.get(view_id).copied()
    }

    /// Create a new xi-core process using already-computed config tables for
    /// the initial buffer. Reusing these tables avoids repeating config and
    /// editorconfig scans during startup.
    pub(crate) fn new_with_initial_config(
        path: Option<PathBuf>,
        general_config: Table,
        initial_overrides: Table,
        lsp_config: Table,
    ) -> io::Result<Self> {
        let (to_core_tx, to_core_rx) = mpsc::channel::<String>(256);
        let (from_core_tx, from_core_rx) = std_mpsc::channel::<String>();
        let (backend_tx, backend_rx) = std_mpsc::channel::<BackendEvent>();

        let core_thread = thread::spawn(move || {
            let mut core = XiCore::new();
            let mut rpc_loop = RpcLoop::new(ChannelWriter { tx: from_core_tx });
            let _ = rpc_loop.mainloop(|| ChannelReader { rx: to_core_rx }, &mut core);
        });

        send_rpc_notification(
            &to_core_tx,
            "client_started",
            json!({
                "config_dir": crate::config::xi_core_config_dir(),
                "client_extras_dir": crate::config::xi_core_client_extras_dir(),
            }),
        )?;

        send_config_notification(&to_core_tx, json!("general"), general_config)?;
        send_config_notification(
            &to_core_tx,
            json!({ "plugin": crate::config::LSP_PLUGIN_NAME }),
            lsp_config,
        )?;

        let new_view_id = 1_u64;
        let new_view_started = Instant::now();
        send_rpc_request(
            &to_core_tx,
            new_view_id,
            "new_view",
            json!({ "file_path": path.as_ref().map(|p| p.to_string_lossy().to_string()) }),
        )?;

        let mut from_core_rx = from_core_rx;
        let view_id_val = block_for_response(&mut from_core_rx, &to_core_tx, new_view_id)?;
        let new_view_rpc = new_view_started.elapsed();
        let view_id = view_id_val
            .as_str()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "new_view returned non-string id")
            })?
            .to_owned();

        send_config_notification(
            &to_core_tx,
            json!({ "user_override": view_id }),
            initial_overrides,
        )?;

        let init_drain_started = Instant::now();
        let init_events = drain_sync_notifications(&mut from_core_rx, &to_core_tx);
        let init_notification_drain = init_drain_started.elapsed();

        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = Arc::clone(&pending);
        let tx_clone = to_core_tx.clone();
        let reader_shutdown = Arc::new(AtomicBool::new(false));
        let reader_shutdown_clone = Arc::clone(&reader_shutdown);
        let reader_thread = thread::spawn(move || {
            xi_reader_thread(
                from_core_rx,
                tx_clone,
                backend_tx,
                pending_clone,
                Some(reader_shutdown_clone),
            )
        });

        let buf = BufState {
            id: 1,
            path: path.clone(),
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
            mtime: path
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok()),
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

        let mut mgr = Self {
            tx: to_core_tx,
            backend_rx,
            core_thread: Some(core_thread),
            reader_thread: Some(reader_thread),
            reader_shutdown,
            bufs: vec![buf],
            view_to_idx,
            current: 0,
            alternate: None,
            access_history: Vec::new(),
            modified_history: Vec::new(),
            next_buf_id: 2,
            next_rpc_id: 2,
            pending,
            pending_locations: Vec::new(),
            pending_symbols: Vec::new(),
            pending_agent_tool_results: Vec::new(),
            available_plugins_by_view: HashMap::new(),
            pending_ui_actions: Vec::new(),
            startup_profile: StartupProfile {
                new_view_rpc,
                init_notification_drain,
                ..StartupProfile::default()
            },
            startup_profile_active: true,
            vlf_viewports: VlfViewportScheduler::default(),
        };

        let init_apply_started = Instant::now();
        for event in init_events {
            mgr.apply_event_to_buffer(event)?;
        }
        mgr.startup_profile.init_event_apply = init_apply_started.elapsed();
        let pump_init_started = Instant::now();
        mgr.pump_init()?;
        mgr.startup_profile.pump_init = pump_init_started.elapsed();
        mgr.startup_profile_active = false;
        Ok(mgr)
    }

    #[cfg(test)]
    pub(crate) fn startup_profile(&self) -> &StartupProfile {
        &self.startup_profile
    }

    pub(crate) fn active(&self) -> &BufState {
        &self.bufs[self.current]
    }

    pub(crate) fn set_buffer_path(&mut self, id: BufferId, path: PathBuf) -> io::Result<()> {
        let idx = self
            .bufs
            .iter()
            .position(|buf| buf.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        let mtime = std::fs::metadata(&path).ok().and_then(|meta| meta.modified().ok());
        let buf = &mut self.bufs[idx];
        buf.path = Some(path);
        buf.display_name = None;
        buf.mtime = mtime;
        buf.externally_modified = false;
        buf.editor_config_synced = false;
        Ok(())
    }

    /// Slice of all open buffers (for window/UI enumeration).
    pub(crate) fn all_bufs(&self) -> &[BufState] {
        &self.bufs
    }

    pub(crate) fn buf_count(&self) -> usize {
        self.bufs.len()
    }

    pub(crate) fn current_idx(&self) -> usize {
        self.current
    }
}
