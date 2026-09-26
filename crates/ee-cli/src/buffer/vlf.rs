//! `impl BufferManager` methods: vlf.
use super::*;

impl BufferManager {
    pub(crate) fn notify_scroll(&mut self, first_line: usize, last_line: usize) -> io::Result<()> {
        // Stage A Phase 3: VLF and rope buffers share one content channel —
        // `scroll` → core render → `update`. The core renders the visible
        // window only, so no overscan prefetch is needed here.
        let range = (first_line, last_line);
        let buf = &mut self.bufs[self.current];
        if buf.view_id.is_empty() {
            return Ok(());
        }
        buf.vlf_requested_viewport = Some(range);
        if buf.last_scroll == Some(range) {
            return Ok(());
        }
        buf.last_scroll = Some(range);
        send_xi_notification(
            &self.tx,
            "edit",
            json!({
                "view_id": buf.view_id,
                "method": "scroll",
                "params": [first_line, last_line],
            }),
        )
    }

    pub(crate) fn force_vlf_viewport_refresh(
        &mut self,
        first_line: usize,
        last_line: usize,
    ) -> io::Result<()> {
        // Region edits force a repaint; the unified scroll path dedupes on the
        // last range, so clear it first.
        self.bufs[self.current].last_scroll = None;
        self.notify_scroll(first_line, last_line)
    }
    pub(crate) fn request_vlf_tail_viewport(&mut self, viewport_lines: usize) -> io::Result<()> {
        // Goto-end: ask the core to scroll to the last screen. The core clamps
        // the viewport to the file tail on the next render; when the window
        // update lands, `apply_vlf_update_window` completes the cursor jump.
        // The sentinel carries only a viewport-sized span: a sentinel sized by
        // the line count wedges the view height into the hundreds of
        // thousands, and later `request_lines` repairs then render whole-file
        // spans (hundreds of ms each).
        let buf = &mut self.bufs[self.current];
        if !buf.is_vlf || buf.view_id.is_empty() {
            return Ok(());
        }
        let height = i64::try_from(viewport_lines.max(1)).unwrap_or(i64::MAX);
        let first = 9_000_000_000_i64.saturating_sub(height);
        let last = first.saturating_add(height);
        buf.pending_vlf_tail_jump = true;
        // The jump owns the cursor from here on: the arm-time position is what
        // the user must not drift from (see `abandon_tail_jump_if_user_navigated`).
        buf.vlf_tail_jump_cursor = Some(buf.cursor_line);
        buf.vlf_tail_jump_viewport = Some(viewport_lines);
        buf.last_scroll = None;
        send_xi_notification(
            &self.tx,
            "edit",
            json!({
                "view_id": buf.view_id,
                "method": "scroll",
                "params": [first, last],
            }),
        )
    }

    pub(crate) fn cancel_vlf_tail_jump(&mut self) {
        let buf = &mut self.bufs[self.current];
        let had_jump = buf.pending_vlf_tail_jump || buf.vlf_tail_jump_cursor.is_some();
        buf.pending_vlf_tail_jump = false;
        buf.vlf_tail_jump_cursor = None;
        buf.vlf_tail_jump_viewport = None;
        if had_jump {
            // Drop the landed-window dedupe key: the user's viewport must be
            // re-requested instead of the stale tail window. Skipped when there
            // was no jump so ordinary navigation (and non-VLF buffers) keep the
            // per-frame scroll dedupe.
            buf.last_scroll = None;
        }
    }

    pub(crate) fn apply_local_vlf_replace_range(
        &mut self,
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
        text: &str,
    ) -> bool {
        self.bufs[self.current]
            .apply_local_vlf_replace_range(start_line, start_col, end_line, end_col, text)
    }
    pub(crate) fn drain_events(&mut self) -> io::Result<()> {
        let mut events = Vec::new();
        while let Ok(event) = self.backend_rx.try_recv() {
            events.push(event);
        }
        for event in coalesce_backend_events(events) {
            self.apply_event_to_buffer(event)?;
        }
        Ok(())
    }
}
