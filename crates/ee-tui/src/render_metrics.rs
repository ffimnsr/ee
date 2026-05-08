/// Lightweight counters for render-path observability.
///
/// Counters are tracked per-session and are intended for:
/// - regression tests (asserting render_invalidation_count stays bounded)
/// - performance budget tests (render_count × per-frame deadline)
///
/// All methods are `#[inline]` so the cost in release builds is negligible.
#[derive(Debug, Default, Clone)]
pub(crate) struct RenderMetrics {
    /// Total number of completed `terminal.draw()` calls.
    pub(crate) render_count: u64,
    /// Number of `CoreUpdateKind::Invalidate` operations received across all
    /// buffers since metrics were last reset.  High counts indicate excessive
    /// backend churn.
    #[allow(dead_code)] // wiring planned: BufState::apply_update Invalidate branch
    pub(crate) invalidation_count: u64,
    /// Bytes written to the line cache before the first render frame.
    /// Useful for diagnosing whether a full-buffer clone happened on open.
    #[allow(dead_code)] // wiring planned: rebuild_lines pre-render accounting
    pub(crate) bytes_before_first_render: u64,
    /// Set to `true` after the first render frame completes.
    first_render_done: bool,
}

impl RenderMetrics {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record one completed render frame.
    #[inline]
    pub(crate) fn record_render(&mut self) {
        self.first_render_done = true;
        self.render_count += 1;
    }

    /// Record `n` invalidate ops received from the backend.
    #[inline]
    #[allow(dead_code)] // wiring planned
    pub(crate) fn record_invalidations(&mut self, n: u64) {
        self.invalidation_count += n;
    }

    /// Accumulate `n` bytes seen in the line cache before the first render.
    #[inline]
    #[allow(dead_code)] // wiring planned
    pub(crate) fn record_pre_render_bytes(&mut self, n: u64) {
        if !self.first_render_done {
            self.bytes_before_first_render += n;
        }
    }

    /// Reset all counters.
    #[inline]
    #[allow(dead_code)] // wiring planned
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_count_starts_at_zero() {
        let m = RenderMetrics::new();
        assert_eq!(m.render_count, 0);
    }

    #[test]
    fn record_render_increments_count() {
        let mut m = RenderMetrics::new();
        m.record_render();
        m.record_render();
        assert_eq!(m.render_count, 2);
    }

    #[test]
    fn record_invalidations_accumulates() {
        let mut m = RenderMetrics::new();
        m.record_invalidations(10);
        m.record_invalidations(5);
        assert_eq!(m.invalidation_count, 15);
    }

    #[test]
    fn pre_render_bytes_only_counted_before_first_render() {
        let mut m = RenderMetrics::new();
        m.record_pre_render_bytes(1024);
        assert_eq!(m.bytes_before_first_render, 1024);
        m.record_render();
        m.record_pre_render_bytes(512);
        // Must not accumulate after the first render.
        assert_eq!(m.bytes_before_first_render, 1024);
    }

    #[test]
    fn reset_clears_all_counters() {
        let mut m = RenderMetrics::new();
        m.record_render();
        m.record_invalidations(7);
        m.record_pre_render_bytes(256);
        m.reset();
        assert_eq!(m.render_count, 0);
        assert_eq!(m.invalidation_count, 0);
        assert_eq!(m.bytes_before_first_render, 0);
    }
}
