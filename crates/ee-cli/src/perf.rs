use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::app::App;
use crate::backend::LineSlot;
use crate::ui::ui;

const DEFAULT_VIEWPORT_LINES: usize = 40;
const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Debug, Clone)]
pub struct OpenToFirstRenderMetrics {
    pub open: Duration,
    pub draw: Duration,
    pub total: Duration,
    pub is_vlf: bool,
}

#[derive(Debug, Clone)]
pub struct PageDownMetrics {
    pub cold: Duration,
    pub warm: Duration,
    pub viewport_lines: usize,
    pub target_top_line: usize,
}

/// §4 gate: per-render latency at head / middle / tail of the fixture, plus
/// the index-behind (`Pending`) window, all in the unified `update` pipeline.
#[derive(Debug, Clone)]
pub struct RenderPositionMetrics {
    /// Latency of the first render while the index is still `Pending` (empty
    /// window replied immediately; measured by the applied-update counter).
    pub pending: Duration,
    /// Warm head / middle / tail renders (index exact + decode cache warm),
    /// min over repetitions to defeat the 5 ms poll quantization.
    pub head: Duration,
    pub middle: Duration,
    pub tail: Duration,
    /// Seconds spent waiting for the background index to finish (scan_all).
    pub index_wait: Duration,
    pub total_lines: u64,
    pub viewport_lines: usize,
}

pub fn measure_open_to_first_render(path: &Path) -> io::Result<OpenToFirstRenderMetrics> {
    let started = Instant::now();
    let app = App::from_path(Some(path.to_path_buf()))?;
    let open = started.elapsed();

    let backend = TestBackend::new(120, 50);
    let mut terminal = Terminal::new(backend).map_err(io_other)?;
    let draw_started = Instant::now();
    terminal.draw(|frame| ui(frame, &app)).map_err(io_other)?;
    let draw = draw_started.elapsed();

    Ok(OpenToFirstRenderMetrics { open, draw, total: open + draw, is_vlf: app.backend.is_vlf })
}

pub fn measure_vlf_page_down(path: &Path, settle_timeout: Duration) -> io::Result<PageDownMetrics> {
    let viewport_lines = DEFAULT_VIEWPORT_LINES;
    let mut app = open_vlf_app(path, viewport_lines, settle_timeout)?;

    ensure_viewport_ready(&mut app, viewport_lines, settle_timeout)?;

    let cold = measure_page_motion(&mut app, KeyCode::PageDown, viewport_lines, settle_timeout)?;
    let target_top_line = app.viewport.top_line;

    let _ = measure_page_motion(&mut app, KeyCode::PageUp, viewport_lines, settle_timeout)?;
    if app.viewport.top_line != 0 {
        return Err(io::Error::other(format!(
            "expected page-up to return to top before warm sample, got {}",
            app.viewport.top_line
        )));
    }

    let warm = measure_page_motion(&mut app, KeyCode::PageDown, viewport_lines, settle_timeout)?;

    Ok(PageDownMetrics { cold, warm, viewport_lines, target_top_line })
}

/// §4 gate driver (app-level, end-to-end): unified per-render latency at
/// head / middle / tail on a warm fixture plus the pre-index `Pending`
/// reply. Repetitions are min-aggregated so the 5 ms pump quantization does
/// not inflate the budget sample.
///
/// The end-to-end numbers ALSO include the app's transport polls (core+reader
/// channel quanta), so the gate's pass criteria use the store-level
/// [`measure_store_render_positions`] instead; this stays as the reported
/// user-visible latency.
pub fn measure_vlf_render_positions(
    path: &Path,
    settle_timeout: Duration,
    viewport_lines: usize,
    repetitions: usize,
) -> io::Result<RenderPositionMetrics> {
    let mut app = open_vlf_app(path, viewport_lines, settle_timeout)?;

    // Index-behind renders: the index has not produced anything yet, so the
    // empty window must still ride the `update` channel promptly. Min over
    // samples; the first reply includes open-cost noise.
    let mut pending = Duration::MAX;
    for _ in 0..repetitions {
        pending = pending.min(measure_pending_render(&mut app, viewport_lines, settle_timeout)?);
    }

    // Let the background index finish; line lookups become exact after this.
    // The exact flag only propagates through render updates, so the wait
    // heartbeats a forced head render until the payload reports exact.
    let index_started = Instant::now();
    wait_vlf_index_exact(&mut app, viewport_lines, settle_timeout)?;
    let index_wait = index_started.elapsed();

    // Warm the decode cache at head; two passes so the second lands on the
    // cached seam.
    vlf_scroll_render(&mut app, 0, viewport_lines, settle_timeout)?;
    let _ = vlf_scroll_render(&mut app, 0, viewport_lines, settle_timeout)?;

    let total_lines = app.backend.vlf_approx_line_count as u64;
    let middle_top = total_lines.saturating_sub(viewport_lines as u64) / 2;

    let mut best_head = Duration::MAX;
    let mut best_middle = Duration::MAX;
    let mut best_tail = Duration::MAX;

    // Alternate target positions across repetitions so every sample hits a
    // freshly requested window (the backend dedupes identical scroll ranges).
    for rep in 0..repetitions {
        let head_shift = if rep.is_multiple_of(2) { 0 } else { 2 };
        best_head =
            best_head.min(vlf_scroll_render(&mut app, head_shift, viewport_lines, settle_timeout)?);
        best_middle = best_middle.min(vlf_scroll_render(
            &mut app,
            (middle_top as usize).saturating_add(if rep.is_multiple_of(2) { 0 } else { 2 }),
            viewport_lines,
            settle_timeout,
        )?);
        best_tail = best_tail.min(measure_tail_render(&mut app, viewport_lines, settle_timeout)?);
    }

    Ok(RenderPositionMetrics {
        pending,
        head: best_head,
        middle: best_middle,
        tail: best_tail,
        index_wait,
        total_lines,
        viewport_lines,
    })
}

/// §4 gate driver (store-level): the actual per-render work the spec gates
/// on — decode + index + line lookups for a `viewport_lines` window at head /
/// middle / tail, plus the index-behind (`Pending`) window before any page is
/// scanned. Times the facade pattern (`line_to_byte` × 2 + interval read per
/// line) exactly as the render loop does, with no transport in the way.
pub fn measure_store_render_positions(
    path: &Path,
    viewport_lines: usize,
    _settle_timeout: Duration,
) -> io::Result<RenderPositionMetrics> {
    use xi_core_lib::text_store::TextStore;
    use xi_core_lib::vlf::store::VlfStore;

    let store = VlfStore::open_with_config(
        path,
        crate::vlf_bench_support::PAGE_SIZE,
        crate::vlf_bench_support::DECODED_BUDGET,
    )?;
    store.enable_editing();
    let store = &store;

    // Index-behind sample: nothing scanned yet; every lookup walks, so this
    // is the pre-index per-render cost (window-bounded, but with the walk's
    // per-line slab reads).
    let pending = {
        let mut best = Duration::MAX;
        for _ in 0..3 {
            let t = Instant::now();
            render_window_work(store, 0, viewport_lines)?;
            best = best.min(t.elapsed());
        }
        best
    };

    // Complete the index synchronously (the background thread and the bench
    // share the same scan path; `scan_all` keeps the measure deterministic).
    let index_started = Instant::now();
    store.scan_all()?;
    let index_wait = index_started.elapsed();

    let total_lines = match store.exact_logical_line_count_streaming() {
        Ok(n) => n,
        Err(_) => store.len_bytes() / 80 + 1,
    };
    let middle_top = total_lines.saturating_sub(viewport_lines as u64) / 2;
    let tail_top = total_lines.saturating_sub(viewport_lines as u64 + 1);

    let measure = |start_line: u64| -> io::Result<Duration> {
        let mut best = Duration::MAX;
        for _ in 0..3 {
            let t = Instant::now();
            render_window_work(store, start_line, viewport_lines)?;
            best = best.min(t.elapsed());
        }
        Ok(best)
    };

    Ok(RenderPositionMetrics {
        pending,
        head: measure(0)?,
        middle: measure(middle_top)?,
        tail: measure(tail_top)?,
        index_wait,
        total_lines,
        viewport_lines,
    })
}

/// One window of the render's per-line work: two line→byte lookups (the
/// facade's start/next pairing) plus the line-interval read.
fn render_window_work(
    store: &xi_core_lib::vlf::store::VlfStore,
    start_line: u64,
    viewport_lines: usize,
) -> io::Result<()> {
    use xi_core_lib::text_store::{LineLookup, LogicalLine, TextStore};
    let mut prev_end = None;
    for l in start_line..start_line + viewport_lines as u64 {
        let a = match store.line_to_byte(LogicalLine(l)) {
            LineLookup::Exact(o) => o.0,
            LineLookup::Approximate(o) => o.0,
            _ => continue,
        };
        let b = match store.line_to_byte(LogicalLine(l + 1)) {
            LineLookup::Exact(o) => o.0,
            _ => TextStore::len_bytes(store),
        };
        if prev_end.is_some() {
            let _ =
                TextStore::read_byte_range(store, xi_core_lib::text_store::ByteRange::new(a, b));
        }
        prev_end = Some(b);
    }
    Ok(())
}

/// §4 gate alloc probe: peak live bytes per warm render at head / middle /
/// tail, tracked by the lib's counting allocator (armed only for the
/// sample, so normal operation is untouched).
#[derive(Debug, Clone)]
pub struct RenderAllocMetrics {
    pub head: u64,
    pub middle: u64,
    pub tail: u64,
    pub viewport_lines: usize,
}

impl RenderAllocMetrics {
    pub fn samples(&self) -> [(&'static str, u64); 3] {
        [("head", self.head), ("middle", self.middle), ("tail", self.tail)]
    }
}

/// §4 gate alloc probe: open a warm app once, then measure the peak live
/// allocation of one scroll-render at each position with the counting
/// allocator armed.
pub fn measure_vlf_render_allocations(
    path: &Path,
    settle_timeout: Duration,
    viewport_lines: usize,
) -> io::Result<RenderAllocMetrics> {
    let mut app = open_vlf_app(path, viewport_lines, settle_timeout)?;
    wait_vlf_index_exact(&mut app, viewport_lines, settle_timeout)?;
    vlf_scroll_render(&mut app, 0, viewport_lines, settle_timeout)?;

    let total_lines = app.backend.vlf_approx_line_count as u64;
    let middle_top = total_lines.saturating_sub(viewport_lines as u64) / 2;

    let sample = |app: &mut App, top: usize| -> io::Result<u64> {
        crate::bench_alloc::arm();
        let result = vlf_scroll_render(app, top, viewport_lines, settle_timeout);
        let peak = crate::bench_alloc::peak_bytes();
        crate::bench_alloc::disarm();
        result?;
        Ok(peak)
    };
    let sample_tail = |app: &mut App| -> io::Result<u64> {
        crate::bench_alloc::arm();
        let result = measure_tail_render(app, viewport_lines, settle_timeout);
        let peak = crate::bench_alloc::peak_bytes();
        crate::bench_alloc::disarm();
        result?;
        Ok(peak)
    };

    let head = sample(&mut app, 0)?;
    let middle = sample(&mut app, middle_top as usize)?;
    let tail = sample_tail(&mut app)?;

    Ok(RenderAllocMetrics { head, middle, tail, viewport_lines })
}

/// Scroll `top..top+viewport` through the real pipeline and wait until the
/// window slots are Known. Returns the end-to-end latency.
pub(crate) fn vlf_scroll_render(
    app: &mut App,
    top: usize,
    viewport_lines: usize,
    settle_timeout: Duration,
) -> io::Result<Duration> {
    let started = Instant::now();
    app.viewport.top_line = top;
    request_visible_viewport(app, viewport_lines)?;
    ensure_viewport_ready(app, viewport_lines, settle_timeout)?;
    Ok(started.elapsed())
}

/// Tail render: fire `request_vlf_tail_viewport` (huge sentinel), wait for
/// the jump to land (`pending_vlf_tail_jump` cleared — the sentinel window
/// itself is slop-sized), then measure the normal tail render at the
/// landing position, which is the real per-render cost at the tail.
pub(crate) fn measure_tail_render(
    app: &mut App,
    viewport_lines: usize,
    settle_timeout: Duration,
) -> io::Result<Duration> {
    app.backend.request_vlf_tail_viewport(viewport_lines)?;
    pump_until(app, settle_timeout, |app| !app.backend.pending_vlf_tail_jump)?;
    let top = app.viewport.top_line;
    if top == 0 {
        return Err(io::Error::other("tail jump did not move the viewport"));
    }
    vlf_scroll_render(app, top, viewport_lines, settle_timeout)
}

/// Wait until the background VLF page index is complete (`index_progress`
/// 1.0). The progress flag rides render updates only, so each poll forces a
/// head render (the backend dedupes identical scroll ranges otherwise).
///
/// Note: `vlf_line_count_exact` is NOT a proxy for this — it is set by the
/// streaming LF count as soon as the first update lands, while the page
/// index (needed for exact base lookups) still scans.
pub(crate) fn wait_vlf_index_exact(
    app: &mut App,
    viewport_lines: usize,
    timeout: Duration,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if app.backend.vlf_index_progress >= 1.0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for VLF index to finish",
            ));
        }
        app.backend.force_vlf_viewport_refresh(
            app.viewport.top_line,
            app.viewport.top_line + viewport_lines,
        )?;
        app.backend.sync_pending_events()?;
        thread::sleep(POLL_INTERVAL);
    }
}

/// Pre-index render: the reply is an empty `update` (no content to wait
/// for), so completion is the applied-update counter.
fn measure_pending_render(
    app: &mut App,
    viewport_lines: usize,
    settle_timeout: Duration,
) -> io::Result<Duration> {
    let started = Instant::now();
    let before = app.backend.render_updates;
    request_visible_viewport(app, viewport_lines)?;
    pump_until(app, settle_timeout, |app| app.backend.render_updates > before)?;
    Ok(started.elapsed())
}

fn open_vlf_app(path: &Path, viewport_lines: usize, settle_timeout: Duration) -> io::Result<App> {
    let mut app = App::from_path(Some(path.to_path_buf()))?;
    app.last_editor_height = viewport_lines;

    pump_until(&mut app, settle_timeout, |app| app.backend.is_vlf)?;
    if !app.backend.is_vlf {
        return Err(io::Error::other(format!("expected VLF open for {}", path.display())));
    }

    Ok(app)
}

fn measure_page_motion(
    app: &mut App,
    key: KeyCode,
    viewport_lines: usize,
    settle_timeout: Duration,
) -> io::Result<Duration> {
    let started = Instant::now();
    app.handle_event(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)));
    request_visible_viewport(app, viewport_lines)?;
    ensure_viewport_ready(app, viewport_lines, settle_timeout)?;
    Ok(started.elapsed())
}

fn ensure_viewport_ready(
    app: &mut App,
    viewport_lines: usize,
    settle_timeout: Duration,
) -> io::Result<()> {
    request_visible_viewport(app, viewport_lines)?;
    pump_until(app, settle_timeout, |app| {
        !app.backend.pending_line_request && viewport_ready(app, viewport_lines)
    })
    .map_err(|err| {
        io::Error::other(format!(
            "{err} (top={}, viewport={viewport_lines})",
            app.viewport.top_line
        ))
    })
}

fn request_visible_viewport(app: &mut App, viewport_lines: usize) -> io::Result<()> {
    let top = app.viewport.top_line;
    // Force: the backend dedupes identical scroll ranges, and benchmark
    // samples re-request the same window back-to-back (head reps, the index
    // wait heartbeat). Every request must produce a fresh render.
    app.backend.force_vlf_viewport_refresh(top, top.saturating_add(viewport_lines))
}

fn viewport_ready(app: &App, viewport_lines: usize) -> bool {
    let top = app.viewport.top_line;
    (top..top.saturating_add(viewport_lines))
        .all(|idx| matches!(app.backend.line_slot(idx), Some(LineSlot::Known(_))))
}

fn pump_until(
    app: &mut App,
    timeout: Duration,
    mut predicate: impl FnMut(&App) -> bool,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate(app) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for VLF benchmark condition",
            ));
        }
        app.backend.sync_pending_events()?;
        thread::sleep(POLL_INTERVAL);
    }
}

fn io_other(err: impl std::fmt::Display) -> io::Error {
    io::Error::other(err.to_string())
}
