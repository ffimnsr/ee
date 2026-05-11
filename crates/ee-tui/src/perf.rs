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
}

fn request_visible_viewport(app: &mut App, viewport_lines: usize) -> io::Result<()> {
    let top = app.viewport.top_line;
    app.backend.notify_scroll(top, top.saturating_add(viewport_lines))
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
