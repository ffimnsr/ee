//! `impl BufferManager` methods: events.
use super::*;

impl BufferManager {
    pub(crate) fn pump_init(&mut self) -> io::Result<()> {
        let mut idle_rounds = 0;
        loop {
            if startup_render_ready(&self.bufs[self.current].line_cache) {
                break;
            }
            if self.bufs[self.current].is_vlf
                && self.bufs[self.current].last_scroll.is_none()
                && !self.bufs[self.current].pending_line_request
            {
                self.notify_scroll(0, Self::STARTUP_VLF_VIEWPORT_LINES)?;
            }
            if invalid_line_ranges(&self.bufs[self.current].line_cache).is_empty()
                && !self.current_buffer_may_receive_initial_content()
            {
                break;
            }
            match recv_with_timeout(&mut self.backend_rx, Duration::from_millis(20)) {
                Some(event) => {
                    let mut saw_critical = event.is_startup_critical();
                    self.apply_event_to_buffer(event)?;
                    while let Ok(event) = self.backend_rx.try_recv() {
                        saw_critical |= event.is_startup_critical();
                        self.apply_event_to_buffer(event)?;
                    }

                    if saw_critical {
                        idle_rounds = 0;
                    } else {
                        idle_rounds += 1;
                        if idle_rounds >= Self::SYNC_IDLE_LIMIT {
                            break;
                        }
                    }
                }
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

    pub(super) fn current_buffer_may_receive_initial_content(&self) -> bool {
        let buf = &self.bufs[self.current];
        buf.pending_line_request
            || buf
                .path
                .as_ref()
                .and_then(|path| std::fs::metadata(path).ok())
                .is_some_and(|metadata| metadata.len() > 0)
    }

    /// Re-request a VLF viewport when an update exposes index maturity the
    /// previous scroll missed:
    ///
    /// - The open scroll can be answered before the background index reports
    ///   a line count (the core's "nothing changed" shortcut emits a no-op
    ///   copy). Because the count only advances when the store renders, the
    ///   frontend must force a real render: `request_lines` upgrades the plan
    ///   to Render unconditionally. Each response advances the index and the
    ///   cycle stops once rows land (or the store reports an exact zero).
    /// - A goto-end sentinel can land on the *approximate* count while the
    ///   index still scans; keeping the window at the old tail leaves the
    ///   cursor short of the true end. While the count is inexact and the
    ///   window touches the current tail, re-issue the tail request until the
    ///   index completes, and re-jump when the matured count moves the tail
    ///   past a cursor parked on a stale (approximate) end.
    ///
    /// Tail chasing is intent-bound: a tail landing answers a sentinel, and a
    /// stale tail window is only re-driven while the user's viewport is still
    /// at the tail. Either way the jump yields to user navigation, so a pending
    /// or stale tail must never starve the visible viewport of its rows.
    fn vlf_after_update_refresh(
        &mut self,
        idx: usize,
        tail_pending_before: bool,
        count_before: u64,
        exact_before: bool,
    ) -> io::Result<()> {
        if idx != self.current {
            return Ok(());
        }
        let (
            count,
            exact,
            start,
            len,
            requested_viewport,
            view_id,
            unpopulated,
            tail_jump_cleared,
            cursor,
        ) = {
            let buf = &self.bufs[idx];
            if !buf.is_vlf || buf.view_id.is_empty() {
                return Ok(());
            }
            (
                buf.vlf_approx_line_count,
                buf.vlf_line_count_exact,
                buf.vlf_cache_start_line,
                buf.line_cache.len(),
                buf.vlf_requested_viewport,
                buf.view_id.clone(),
                buf.line_cache.iter().all(|slot| matches!(slot, LineSlot::Invalid)),
                tail_pending_before && !buf.pending_vlf_tail_jump,
                buf.cursor_line,
            )
        };
        // A tail jump just landed: the sentinel only renders the slop rows at
        // the true end, so preload the page above the tail (the viewport the
        // jump cursor sits at) on the next update.
        if tail_jump_cleared && let Some(height) = self.bufs[idx].vlf_tail_jump_viewport.take() {
            let cursor = self.bufs[idx].cursor_line;
            let last = cursor.saturating_add(1);
            let first = last.saturating_sub(height);
            return self.notify_scroll(first, last);
        }
        let count_usize = usize::try_from(count).unwrap_or(usize::MAX);
        // The tail jump is still pending: the last sentinel render landed no
        // window (e.g. it clamped onto an approximate total that overshot the
        // real line count and produced zero rows). Re-drive with the freshest
        // count until a window lands.
        if !tail_jump_cleared && self.bufs[idx].pending_vlf_tail_jump {
            if count > 0 {
                // Pass a viewport-sized height: the sentinel's span must not
                // travel with the line count (it wedges the core view height).
                let height = self.bufs[idx]
                    .vlf_tail_jump_viewport
                    .unwrap_or(Self::STARTUP_VLF_VIEWPORT_LINES);
                return self.request_vlf_tail_viewport(height);
            }
            return Ok(());
        }
        if unpopulated {
            if exact && count == 0 {
                return Ok(()); // the store really has no lines; nothing renders
            }
            let (first, last) = requested_viewport.unwrap_or((0, Self::STARTUP_VLF_VIEWPORT_LINES));
            return send_rpc_notification(
                &self.tx,
                "edit",
                json!({
                    "view_id": view_id,
                    "method": "request_lines",
                    "params": [first, last],
                }),
            );
        }
        // The index matured while a previous jump left the cursor parked on
        // the stale (approximate) end: re-jump so the tail chases the real
        // end of the file. `count_before > 0` skips the open-time 0 -> N
        // transition, which is not a tail move. Checked before the stale-window
        // re-drive below: a matured count is the stronger signal, and the two
        // can apply to the same update.
        let count_matured = count != count_before || exact != exact_before;
        if count_matured
            && count_before > 0
            && count > 0
            && cursor.saturating_add(1) >= usize::try_from(count_before).unwrap_or(usize::MAX)
        {
            let height =
                self.bufs[idx].vlf_tail_jump_viewport.unwrap_or(Self::STARTUP_VLF_VIEWPORT_LINES);
            return self.request_vlf_tail_viewport(height);
        }
        // An inexact window sitting on the tail is only worth re-driving while
        // the user's viewport is at the tail: otherwise the window is a stale
        // tail landing and re-driving it would override the user's scroll.
        let requested_at_tail = requested_viewport.is_some_and(|(_, last)| last >= count_usize);
        if !exact && count > 0 && requested_at_tail && start.saturating_add(len) >= count_usize {
            let height =
                self.bufs[idx].vlf_tail_jump_viewport.unwrap_or(Self::STARTUP_VLF_VIEWPORT_LINES);
            return self.request_vlf_tail_viewport(height);
        }
        // The bounded window can miss viewport rows the core still considers
        // cached (its shadow is document-wide, and deep scrolling shrinks the
        // window): without a forced re-render the core answers scrolls with
        // copy-only updates and the rows stay `Loading`. Request the uncovered
        // ranges; `pending_line_request` gates the re-sends until a response
        // lands.
        if !self.bufs[idx].pending_line_request
            && let Some((first, last)) = requested_viewport
        {
            let mut missing: Vec<(usize, usize)> = Vec::new();
            {
                let buf = &self.bufs[idx];
                let mut gap_start: Option<usize> = None;
                for li in first..last {
                    let covered = matches!(buf.line_slot(li), Some(LineSlot::Known(_)));
                    match (covered, gap_start) {
                        (false, None) => gap_start = Some(li),
                        (true, Some(start)) => {
                            missing.push((start, li));
                            gap_start = None;
                        }
                        _ => {}
                    }
                }
                if let Some(start) = gap_start {
                    missing.push((start, last));
                }
            }
            if !missing.is_empty() {
                self.bufs[idx].pending_line_request = true;
                for (start, end) in missing.into_iter().take(4) {
                    send_rpc_notification(
                        &self.tx,
                        "edit",
                        json!({
                            "view_id": view_id,
                            "method": "request_lines",
                            "params": [start, end],
                        }),
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Abandon a pending tail jump when the user navigated away from it.
    ///
    /// The VLF cursor is frontend-authoritative: navigation moves it locally and
    /// never reaches the core, so a cursor that no longer matches the line the
    /// jump owns means the user is looking elsewhere. The jump must then yield —
    /// otherwise its sentinel re-drives on every update, pulling the window back
    /// to the tail (rows render `Loading`) and blocking the coverage repair.
    fn abandon_tail_jump_if_user_navigated(&mut self, idx: usize) {
        let abandoned = {
            let buf = &self.bufs[idx];
            buf.pending_vlf_tail_jump
                && buf.vlf_tail_jump_cursor.is_some_and(|owned| owned != buf.cursor_line)
        };
        if abandoned {
            let buf = &mut self.bufs[idx];
            buf.pending_vlf_tail_jump = false;
            buf.vlf_tail_jump_cursor = None;
            buf.vlf_tail_jump_viewport = None;
            // Drop the landed-window dedupe key: the user's viewport must be
            // re-requested so the core renders it again.
            buf.last_scroll = None;
        }
    }

    pub(super) fn apply_event_to_buffer(&mut self, event: BackendEvent) -> io::Result<()> {
        let current = self.current;
        match event {
            BackendEvent::Update { ref view_id, .. }
            | BackendEvent::ScrollTo { ref view_id, .. } => {
                let Some(idx) = self.buffer_index_for_view(view_id) else {
                    return Ok(());
                };
                match event {
                    BackendEvent::Update { update, .. } => {
                        let update_started = Instant::now();
                        let update_pristine = update.pristine;
                        self.abandon_tail_jump_if_user_navigated(idx);
                        let buf = &mut self.bufs[idx];
                        let was_pristine = buf.pristine;
                        buf.pending_line_request = false;
                        let tail_pending_before = buf.pending_vlf_tail_jump;
                        let count_before = buf.vlf_approx_line_count;
                        let exact_before = buf.vlf_line_count_exact;
                        let stats = buf.apply_update(update)?;
                        if was_pristine && update_pristine {
                            self.finish_external_reload(idx);
                        }
                        if self.startup_profile_active {
                            self.startup_profile.update_apply += update_started.elapsed();
                            self.startup_profile.rebuild_lines += stats.rebuild_lines;
                        }
                        // §4 render benchmarks count applied core renders to
                        // measure per-render latency without slot polling.
                        self.render_updates += 1;
                        self.vlf_after_update_refresh(
                            idx,
                            tail_pending_before,
                            count_before,
                            exact_before,
                        )?;
                    }
                    BackendEvent::ScrollTo { line, col, .. } => {
                        let buf = &mut self.bufs[idx];
                        buf.cursor_line = line;
                        buf.cursor_col = col;
                        buf.clamp_cursor();
                    }
                    _ => unreachable!(),
                }

                if !self.bufs[idx].editor_config_synced {
                    let config_sync_started = Instant::now();
                    self.sync_buffer_editor_config(idx)?;
                    if self.startup_profile_active {
                        self.startup_profile.config_sync += config_sync_started.elapsed();
                    }
                }
            }
            BackendEvent::Alert(msg) => {
                self.bufs[current].status_message = Some(msg);
            }
            BackendEvent::Hover { view_id, content } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                self.pending_ui_actions.push(PendingUiAction::Hover { view_id, content });
                self.bufs[idx].status_message = Some(String::from("hover ready"));
            }
            BackendEvent::Completions { view_id, items } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                let count = items.len();
                self.pending_ui_actions.push(PendingUiAction::Completions { view_id, items });
                self.bufs[idx].status_message = Some(if count == 0 {
                    String::from("no completions")
                } else {
                    format!("completions: {count}")
                });
            }
            BackendEvent::Locations { view_id, title, locations } => {
                if self.buffer_index_for_view(&view_id).is_none() {
                    return Ok(());
                }
                // Collect for the App to dispatch to the quickfix list.
                self.pending_locations.push((view_id, title, locations));
            }
            BackendEvent::Symbols { view_id, title, symbols } => {
                if self.buffer_index_for_view(&view_id).is_none() {
                    return Ok(());
                }
                // Collect for the App to dispatch to the symbols picker.
                self.pending_symbols.push((view_id, title, symbols));
            }
            BackendEvent::AvailablePlugins { view_id, plugins } => {
                if self.buffer_index_for_view(&view_id).is_none() {
                    return Ok(());
                }
                self.available_plugins_by_view.insert(view_id, plugins);
            }
            BackendEvent::Diagnostics { view_id, diagnostics } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                let count = diagnostics.len();
                self.bufs[idx].diagnostics = diagnostics;
                self.bufs[idx].status_message = Some(if count == 0 {
                    String::from("diagnostics cleared")
                } else {
                    format!("diagnostics: {count}")
                });
            }
            BackendEvent::CodeActions { view_id, actions } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                let count = actions.len();
                self.pending_ui_actions.push(PendingUiAction::CodeActions { view_id, actions });
                self.bufs[idx].status_message = Some(if count == 0 {
                    String::from("no code actions")
                } else {
                    format!("code actions: {count}")
                });
            }
            BackendEvent::AgentToolResult { view_id, kind, payload } => {
                if self.buffer_index_for_view(&view_id).is_none() {
                    return Ok(());
                }
                self.pending_agent_tool_results.push((view_id, kind, payload));
            }
            BackendEvent::DocumentMode { view_id, is_vlf } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                self.bufs[idx].is_vlf = is_vlf;
                if is_vlf {
                    self.bufs[idx].lines.clear();
                    self.bufs[idx].line_cache =
                        vec![LineSlot::Invalid; Self::STARTUP_VLF_VIEWPORT_LINES];
                    self.bufs[idx].vlf_cache_start_line = 0;
                    self.bufs[idx].pending_line_request = false;
                    self.bufs[idx].last_scroll = None;
                    self.bufs[idx].vlf_approx_line_count = 0;
                    self.bufs[idx].vlf_line_count_exact = false;
                    self.bufs[idx].vlf_index_progress = 0.0;
                    self.bufs[idx].pending_vlf_tail_jump = false;
                    self.bufs[idx].vlf_tail_jump_cursor = None;
                    self.bufs[idx].vlf_tail_jump_viewport = None;
                } else {
                    // Rebuild lines now that mode is set.
                    self.bufs[idx].rebuild_lines();
                }
            }
            BackendEvent::VlfSearchStatus {
                view_id,
                query,
                scanned_bytes,
                total_bytes,
                complete,
                stored_match_count,
                ranges,
            } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                self.bufs[idx].vlf_search_ranges = ranges.clone();
                let preview = ranges
                    .iter()
                    .take(3)
                    .map(|range| {
                        format!("L{}:{}-{}", range.line + 1, range.start_col + 1, range.end_col + 1)
                    })
                    .collect::<Vec<_>>();
                let progress = format!("{}/{} B", scanned_bytes, total_bytes);
                self.bufs[idx].status_message = Some(if preview.is_empty() {
                    format!(
                        "search {:?}: {} matches, {}{}",
                        query,
                        stored_match_count,
                        progress,
                        if complete { " complete" } else { " scanning" }
                    )
                } else {
                    format!(
                        "search {:?}: {} matches, {}, {}{}",
                        query,
                        stored_match_count,
                        progress,
                        preview.join(", "),
                        if complete { " complete" } else { " scanning" }
                    )
                });
            }
            BackendEvent::SaveProgress { view_id, complete, generation } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                self.bufs[idx].last_save_generation =
                    self.bufs[idx].last_save_generation.max(generation);
                if complete {
                    self.bufs[idx].completed_save_generation =
                        self.bufs[idx].completed_save_generation.max(generation);
                    self.bufs[idx].save_complete = true;
                }
            }
            BackendEvent::SaveResult {
                view_id,
                generation,
                success,
                permission_denied,
                message,
            } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                self.bufs[idx].last_save_result_generation =
                    self.bufs[idx].last_save_result_generation.max(generation);
                self.bufs[idx].last_save_succeeded = success;
                self.bufs[idx].last_save_permission_denied = permission_denied;
                self.bufs[idx].last_save_error_message = message.clone();
                if let Some(message) = message {
                    self.bufs[idx].status_message = Some(message);
                }
            }
        }
        Ok(())
    }

    pub(super) fn finish_external_reload(&mut self, idx: usize) {
        let Some(buf) = self.bufs.get_mut(idx) else {
            return;
        };
        if !buf.externally_modified {
            return;
        }
        let Some(path) = buf.path.as_ref() else {
            return;
        };
        let Some(mtime) = std::fs::metadata(path).ok().and_then(|meta| meta.modified().ok()) else {
            return;
        };
        buf.mtime = Some(mtime);
        buf.externally_modified = false;
        buf.status_message = Some(String::from("reloaded"));
    }

    pub(super) fn request_invalid_lines(
        &mut self,
        idx: usize,
        scope: LineRequestScope,
    ) -> io::Result<()> {
        let Some(buf) = self.bufs.get_mut(idx) else { return Ok(()) };
        if buf.is_vlf || buf.pending_line_request || buf.view_id.is_empty() {
            return Ok(());
        }

        let invalid_ranges = match scope {
            LineRequestScope::WholeDocument => invalid_line_ranges(&buf.line_cache),
            LineRequestScope::Viewport => {
                let (start, end) = bounded_line_request_window(buf);
                invalid_line_ranges_bounded(&buf.line_cache, start, end)
            }
        };
        if invalid_ranges.is_empty() {
            return Ok(());
        }

        let view_id = buf.view_id.clone();
        for (start, end) in invalid_ranges {
            send_rpc_notification(
                &self.tx,
                "edit",
                json!({
                    "view_id": view_id,
                    "method": "request_lines",
                    "params": [start, end],
                }),
            )?;
        }
        buf.pending_line_request = true;
        Ok(())
    }

    pub(super) fn has_pending_line_work(&self, scope: LineRequestScope) -> bool {
        self.bufs.iter().any(|buf| {
            if buf.is_vlf || buf.pending_line_request {
                return buf.pending_line_request;
            }
            match scope {
                LineRequestScope::Viewport => {
                    let (start, end) = bounded_line_request_window(buf);
                    !invalid_line_ranges_bounded(&buf.line_cache, start, end).is_empty()
                }
                LineRequestScope::WholeDocument => !invalid_line_ranges(&buf.line_cache).is_empty(),
            }
        })
    }

    pub(super) fn request_all_invalid_lines(&mut self, scope: LineRequestScope) -> io::Result<()> {
        for idx in 0..self.bufs.len() {
            self.request_invalid_lines(idx, scope)?;
        }
        Ok(())
    }
}
