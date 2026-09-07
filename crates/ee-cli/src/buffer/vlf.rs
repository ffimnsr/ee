//! `impl BufferManager` methods: vlf.
use super::*;

impl BufferManager {
    pub(crate) fn notify_scroll(&mut self, first_line: usize, last_line: usize) -> io::Result<()> {
        let range = (first_line, last_line);
        let buf = &mut self.bufs[self.current];
        if buf.view_id.is_empty() {
            return Ok(());
        }
        let view_id = buf.view_id.clone();

        if buf.is_vlf {
            let visible_lines = last_line.saturating_sub(first_line);
            if buf.pending_vlf_tail_jump {
                if buf.pending_line_request {
                    return Ok(());
                }
                return self.request_vlf_tail_viewport(visible_lines);
            }
            let mut requested_last_line = if visible_lines >= Self::STARTUP_VLF_VIEWPORT_LINES {
                last_line
            } else {
                last_line
                    .saturating_add(Self::VLF_VIEWPORT_OVERSCAN_LINES)
                    .max(first_line.saturating_add(Self::STARTUP_VLF_VIEWPORT_LINES))
            };
            let line_count = buf.line_count();
            if line_count > first_line {
                requested_last_line = requested_last_line.min(line_count);
            }
            let request_range = (first_line, requested_last_line);
            buf.restore_previous_vlf_viewport_if_ready(first_line, requested_last_line);
            if vlf_viewport_ready(buf, first_line, requested_last_line)
                || (buf.last_scroll == Some(request_range) && buf.pending_line_request)
            {
                return Ok(());
            }
            // VLF mode: use the dedicated viewport protocol so the backend
            // only decodes the visible line range from disk. Increment the
            // generation counter so any in-flight response from the previous
            // scroll position is discarded when it arrives.
            let generation = buf.vlf_generation.wrapping_add(1);
            self.vlf_viewports.submit(
                &self.tx,
                VlfViewportRequest::new(
                    view_id,
                    first_line as u64,
                    requested_last_line as u64,
                    generation,
                ),
            )?;
            let buf = &mut self.bufs[self.current];
            buf.last_scroll = Some(request_range);
            buf.vlf_generation = generation;
            buf.pending_line_request = true;
            Ok(())
        } else {
            if buf.last_scroll == Some(range) {
                return Ok(());
            }
            buf.last_scroll = Some(range);
            send_xi_notification(
                &self.tx,
                "edit",
                json!({
                    "view_id": view_id,
                    "method": "scroll",
                    "params": [first_line, last_line],
                }),
            )
        }
    }

    pub(crate) fn force_vlf_viewport_refresh(
        &mut self,
        first_line: usize,
        last_line: usize,
    ) -> io::Result<()> {
        let buf = &mut self.bufs[self.current];
        if !buf.is_vlf {
            return self.notify_scroll(first_line, last_line);
        }

        if buf.view_id.is_empty() {
            return Ok(());
        }
        let generation = buf.vlf_generation.wrapping_add(1);
        let request = VlfViewportRequest::new(
            buf.view_id.clone(),
            first_line as u64,
            last_line as u64,
            generation,
        );
        self.vlf_viewports.submit(&self.tx, request)?;
        let buf = &mut self.bufs[self.current];
        buf.vlf_generation = generation;
        buf.pending_line_request = true;
        buf.last_scroll = Some((first_line, last_line));
        Ok(())
    }
    pub(crate) fn request_vlf_tail_viewport(&mut self, line_count: usize) -> io::Result<()> {
        let buf = &mut self.bufs[self.current];
        if !buf.is_vlf || buf.view_id.is_empty() {
            return Ok(());
        }

        let requested_lines = line_count.max(Self::TAIL_VLF_PREFETCH_LINES);
        let generation = buf.vlf_generation.wrapping_add(1);
        let request = VlfViewportRequest::new(
            buf.view_id.clone(),
            u64::MAX,
            requested_lines.saturating_sub(1) as u64,
            generation,
        );
        self.vlf_viewports.submit(&self.tx, request)?;
        let buf = &mut self.bufs[self.current];
        buf.vlf_generation = generation;
        buf.pending_line_request = true;
        buf.pending_vlf_tail_jump = true;
        buf.last_scroll = None;
        Ok(())
    }

    pub(crate) fn cancel_vlf_tail_jump(&mut self) {
        let buf = &mut self.bufs[self.current];
        if !buf.is_vlf || !buf.pending_vlf_tail_jump {
            return;
        }

        let cancelled_generation = buf.vlf_generation;
        buf.vlf_generation = buf.vlf_generation.wrapping_add(1);
        buf.pending_line_request = false;
        buf.pending_vlf_tail_jump = false;
        buf.last_scroll = None;
        let view_id = buf.view_id.clone();
        self.vlf_viewports.cancel_request(&view_id, cancelled_generation);
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
