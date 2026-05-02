use std::env;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

mod app;
mod backend;
mod buffer;
mod config;
mod folds;
mod highlight;
mod keymap;
mod picker;
mod quickfix;
mod registers;
mod text;
mod ui;
mod window;

#[cfg(test)]
mod tests;

use app::App;
use ui::ui;

/// Install a panic hook that restores the terminal to a sane state before
/// printing the panic message. Without this a panic in raw/alternate-screen
/// mode leaves the terminal unusable.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stderr(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        original(info);
    }));
}

fn main() -> io::Result<()> {
    install_panic_hook();

    // Atomic flag set by SIGTERM and SIGINT handlers so the main loop can
    // exit cleanly instead of being killed mid-draw.
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))
        .map_err(io::Error::other)?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))
        .map_err(io::Error::other)?;

    let path = env::args_os().nth(1).map(Into::into);
    let mut app = App::from_path(path)?;
    run(&mut app, shutdown)
}

fn run(app: &mut App, shutdown: Arc<AtomicBool>) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;

    let result = run_app(&mut terminal, app, shutdown);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    result
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    while !app.should_quit && !shutdown.load(Ordering::Relaxed) {
        app.backend.drain_events()?;
        app.handle_pending_ui_actions();
        // Dispatch pending location results (definition, references, …) to the
        // quickfix list before drawing so the panel opens in the same frame.
        app.handle_pending_locations();
        // Periodically check for external file changes.
        app.backend.check_external_changes();
        // Warn the user when a backing file has been modified externally.
        for buf in app.backend.all_bufs() {
            if buf.externally_modified {
                let title = buf.title();
                app.backend.status_message = Some(format!(
                    "'{title}' changed on disk — use :e! to reload or continue editing"
                ));
                // Only show one warning per frame; the flag stays set until reload.
                break;
            }
        }
        // Write crash-recovery artifacts every ~30 s for modified buffers.
        app.write_recovery_if_due();

        let size = terminal.size()?;
        let term_rect =
            ratatui::layout::Rect { x: 0, y: 0, width: size.width, height: size.height };
        let editor_height = ui::compute_editor_height(term_rect, app);
        app.scroll_into_view(editor_height);
        app.backend.notify_scroll(app.viewport.top_line, app.viewport.top_line + editor_height)?;

        terminal.draw(|frame| ui(frame, app))?;

        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                // SIGWINCH arrives as Event::Resize from crossterm; force a
                // full redraw by clearing the terminal buffer.
                Event::Resize(_, _) => {
                    terminal.clear()?;
                }
                ev => app.handle_event(ev),
            }
        }
    }
    Ok(())
}
