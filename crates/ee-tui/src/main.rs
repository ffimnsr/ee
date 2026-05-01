use std::env;
use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event;
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

mod app;
mod backend;
mod keymap;
mod text;
mod ui;

#[cfg(test)]
mod tests;

use app::App;
use ui::ui;

fn main() -> io::Result<()> {
    let path = env::args_os().nth(1).map(Into::into);
    let mut app = App::from_path(path)?;
    run(&mut app)
}

fn run(app: &mut App) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;

    let result = run_app(&mut terminal, app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    while !app.should_quit {
        app.backend.drain_events()?;

        let size = terminal.size()?;
        let editor_height = (size.height as usize).saturating_sub(2);
        app.scroll_into_view(editor_height);
        app.backend.notify_scroll(app.viewport.top_line, app.viewport.top_line + editor_height)?;

        terminal.draw(|frame| ui(frame, app))?;

        if event::poll(Duration::from_millis(16))? {
            app.handle_event(event::read()?);
        }
    }
    Ok(())
}
