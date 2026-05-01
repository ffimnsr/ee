use std::env;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

mod app;
mod backend;
mod keymap;
mod registers;
mod text;
mod ui;

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
        let _ = execute!(io::stderr(), LeaveAlternateScreen);
        original(info);
    }));
}

fn main() -> io::Result<()> {
    install_panic_hook();

    // Atomic flag set by SIGTERM and SIGINT handlers so the main loop can
    // exit cleanly instead of being killed mid-draw.
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    let path = env::args_os().nth(1).map(Into::into);
    let mut app = App::from_path(path)?;
    run(&mut app, shutdown)
}

fn run(app: &mut App, shutdown: Arc<AtomicBool>) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;

    let result = run_app(&mut terminal, app, shutdown);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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

        let size = terminal.size()?;
        let editor_height = (size.height as usize).saturating_sub(2);
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
