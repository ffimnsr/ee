use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app::{App, Mode, Viewport};
use crate::window::{SplitDir, TabPage, Window, WindowLayout};

const SESSION_VERSION: u32 = 1;
const JUMP_LIST_MAX: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SessionState {
    version: u32,
    buffers: Vec<SessionBuffer>,
    tabs: Vec<SessionTab>,
    focused_tab: usize,
    marks: Vec<SessionMark>,
    jump_list: Vec<SessionCursor>,
    jump_list_idx: usize,
    command_history: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionBuffer {
    path: PathBuf,
    cursor_line: usize,
    cursor_col: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionTab {
    windows: Vec<SessionWindow>,
    focused_window: usize,
    split_dir: SessionSplitDir,
    active_viewport: SessionViewport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionWindow {
    buffer: usize,
    saved_viewport: SessionViewport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionMark {
    name: char,
    line: usize,
    col: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct SessionCursor {
    line: usize,
    col: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct SessionViewport {
    top_line: usize,
    left_col: usize,
    target_col: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SessionSplitDir {
    Horizontal,
    Vertical,
}

impl SessionState {
    pub(crate) fn load() -> io::Result<Option<Self>> {
        let Some(path) = session_file_path() else { return Ok(None) };
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let state: Self = serde_json::from_str(&raw)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        if state.version != SESSION_VERSION {
            return Ok(None);
        }
        Ok(Some(state))
    }

    pub(crate) fn save(app: &App) -> io::Result<()> {
        let Some(path) = session_file_path() else { return Ok(()) };
        let Some(dir) = path.parent() else { return Ok(()) };
        fs::create_dir_all(dir)?;
        let raw = serde_json::to_string_pretty(&Self::capture(app)).map_err(io::Error::other)?;
        fs::write(path, raw)
    }

    pub(crate) fn initial_path(&self) -> Option<PathBuf> {
        self.buffers.first().map(|buffer| buffer.path.clone())
    }

    pub(crate) fn capture(app: &App) -> Self {
        let mut buffer_lookup = HashMap::new();
        let mut buffers = Vec::new();
        for buf in app.backend.all_bufs() {
            let Some(path) = buf.path.as_deref().map(normalize_path) else { continue };
            let idx = buffers.len();
            buffers.push(SessionBuffer {
                path,
                cursor_line: buf.cursor_line,
                cursor_col: buf.cursor_col,
            });
            buffer_lookup.insert(buf.id, idx);
        }

        let focused_tab = app.tabs.focused_idx();
        let mut tabs = Vec::new();
        for (tab_idx, tab) in app.tabs.iter() {
            let mut focused_window = None;
            let mut windows = Vec::new();
            for (window_idx, window) in tab.windows.windows().iter().enumerate() {
                let Some(&buffer_idx) = buffer_lookup.get(&window.buffer_id) else { continue };
                if window_idx == tab.windows.focused_idx() {
                    focused_window = Some(windows.len());
                }
                windows.push(SessionWindow {
                    buffer: buffer_idx,
                    saved_viewport: window.saved_viewport.into(),
                });
            }
            if windows.is_empty() {
                continue;
            }
            tabs.push(SessionTab {
                windows,
                focused_window: focused_window.unwrap_or(0),
                split_dir: tab.windows.split_dir.into(),
                active_viewport: if tab_idx == focused_tab {
                    app.viewport.into()
                } else {
                    tab.saved_viewport.into()
                },
            });
        }

        let mut marks = app
            .marks
            .iter()
            .map(|(&name, &(line, col))| SessionMark { name, line, col })
            .collect::<Vec<_>>();
        marks.sort_by_key(|mark| mark.name);

        Self {
            version: SESSION_VERSION,
            buffers,
            tabs,
            focused_tab,
            marks,
            jump_list: app
                .jump_list
                .iter()
                .map(|&(line, col)| SessionCursor { line, col })
                .collect(),
            jump_list_idx: app.jump_list_idx,
            command_history: app.command_history().to_vec(),
        }
    }

    pub(crate) fn restore(&self, app: &mut App) -> io::Result<()> {
        let mut restored_buffers = Vec::with_capacity(self.buffers.len());
        for (idx, buffer) in self.buffers.iter().enumerate() {
            let buffer_id = if idx == 0
                && app.backend.buf_count() == 1
                && app.backend.active().path.as_ref() == Some(&buffer.path)
            {
                app.backend.active().id
            } else {
                app.backend.open_buffer(Some(buffer.path.clone()))?
            };
            app.backend.restore_cursor(buffer_id, buffer.cursor_line, buffer.cursor_col)?;
            restored_buffers.push(buffer_id);
        }

        let mut restored_tabs = Vec::new();
        let mut focused_tab = None;
        let mut focused_viewport = Viewport::default();
        for (tab_idx, tab) in self.tabs.iter().enumerate() {
            let mut focused_window = None;
            let mut windows = Vec::new();
            for (window_idx, window) in tab.windows.iter().enumerate() {
                let Some(&buffer_id) = restored_buffers.get(window.buffer) else { continue };
                if window_idx == tab.focused_window {
                    focused_window = Some(windows.len());
                }
                windows.push(Window {
                    id: windows.len() as u32 + 1,
                    buffer_id,
                    saved_viewport: window.saved_viewport.into(),
                });
            }
            if windows.is_empty() {
                continue;
            }
            let active_viewport: Viewport = tab.active_viewport.into();
            restored_tabs.push(TabPage {
                windows: WindowLayout::from_parts(
                    windows,
                    focused_window.unwrap_or(0),
                    tab.split_dir.into(),
                ),
                saved_viewport: active_viewport,
            });
            if tab_idx == self.focused_tab {
                focused_tab = Some(restored_tabs.len() - 1);
                focused_viewport = active_viewport;
            }
        }

        if !restored_tabs.is_empty() {
            let focused_tab = focused_tab.unwrap_or(0).min(restored_tabs.len().saturating_sub(1));
            if focused_tab == 0 && focused_viewport == Viewport::default() {
                focused_viewport = restored_tabs[focused_tab].saved_viewport;
            }
            app.tabs.replace_tabs(restored_tabs, focused_tab);
            app.viewport = focused_viewport;
            let active_buffer = app.tabs.focused_windows().focused_window().buffer_id;
            app.backend.switch_to_id(active_buffer)?;
        } else if let Some(&first_buffer) = restored_buffers.first() {
            app.backend.switch_to_id(first_buffer)?;
            app.viewport = Viewport::default();
        }

        app.mode = Mode::Normal;
        app.command_buffer.clear();
        app.restore_command_history(self.command_history.clone());
        app.marks = self.marks.iter().map(|mark| (mark.name, (mark.line, mark.col))).collect();
        app.jump_list = self.jump_list.iter().map(|cursor| (cursor.line, cursor.col)).collect();
        if app.jump_list.len() > JUMP_LIST_MAX {
            let keep_from = app.jump_list.len() - JUMP_LIST_MAX;
            app.jump_list.drain(..keep_from);
        }
        app.jump_list_idx = self.jump_list_idx.min(app.jump_list.len());
        Ok(())
    }
}

impl From<Viewport> for SessionViewport {
    fn from(viewport: Viewport) -> Self {
        Self {
            top_line: viewport.top_line,
            left_col: viewport.left_col,
            target_col: viewport.target_col,
        }
    }
}

impl From<SessionViewport> for Viewport {
    fn from(viewport: SessionViewport) -> Self {
        Self {
            top_line: viewport.top_line,
            left_col: viewport.left_col,
            target_col: viewport.target_col,
        }
    }
}

impl From<SplitDir> for SessionSplitDir {
    fn from(dir: SplitDir) -> Self {
        match dir {
            SplitDir::Horizontal => Self::Horizontal,
            SplitDir::Vertical => Self::Vertical,
        }
    }
}

impl From<SessionSplitDir> for SplitDir {
    fn from(dir: SessionSplitDir) -> Self {
        match dir {
            SessionSplitDir::Horizontal => Self::Horizontal,
            SessionSplitDir::Vertical => Self::Vertical,
        }
    }
}

fn session_file_path() -> Option<PathBuf> {
    dirs::state_dir().or_else(dirs::data_dir).map(|dir| dir.join("ee").join("session.json"))
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(path)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use tempfile::tempdir;

    #[test]
    fn capture_and_restore_session_state() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("one.rs");
        let second = dir.path().join("two.rs");
        fs::write(&first, "fn main() {}\n").unwrap();
        fs::write(&second, "let value = 2;\n").unwrap();

        let mut app = App::from_path(Some(first.clone())).unwrap();
        let second_id = app.backend.open_buffer(Some(second.clone())).unwrap();
        app.backend.switch_to_id(second_id).unwrap();
        let (_, viewport) = app.tabs.focused_windows_mut().split(
            SplitDir::Vertical,
            second_id,
            Viewport { top_line: 7, left_col: 3, target_col: 5 },
        );
        app.viewport = viewport;
        app.tabs
            .new_tab(app.backend.active().id, Viewport { top_line: 7, left_col: 3, target_col: 5 });
        app.viewport = Viewport { top_line: 11, left_col: 2, target_col: 9 };
        app.backend.restore_cursor(app.backend.all_bufs()[0].id, 4, 6).unwrap();
        app.backend.restore_cursor(second_id, 8, 1).unwrap();
        app.marks.insert('a', (3, 2));
        app.jump_list = vec![(1, 0), (9, 4)];
        app.jump_list_idx = 1;
        app.restore_command_history(vec![
            String::from("edit one.rs"),
            String::from("vsplit two.rs"),
        ]);

        let state = SessionState::capture(&app);

        let mut restored = App::from_path(state.initial_path()).unwrap();
        state.restore(&mut restored).unwrap();

        assert_eq!(
            restored.command_history(),
            &[String::from("edit one.rs"), String::from("vsplit two.rs")]
        );
        assert_eq!(restored.marks.get(&'a'), Some(&(3, 2)));
        assert_eq!(restored.jump_list, vec![(1, 0), (9, 4)]);
        assert_eq!(restored.jump_list_idx, 1);
        assert_eq!(restored.tabs.tab_count(), 2);
        assert_eq!(restored.tabs.focused_idx(), 1);
        assert_eq!(restored.viewport, Viewport { top_line: 11, left_col: 2, target_col: 9 });
        assert_eq!(restored.backend.all_bufs().len(), 2);
        assert_eq!(restored.backend.all_bufs()[0].cursor_line, 4);
        assert_eq!(restored.backend.all_bufs()[0].cursor_col, 6);
        assert_eq!(restored.backend.all_bufs()[1].cursor_line, 8);
        assert_eq!(restored.backend.all_bufs()[1].cursor_col, 1);
        assert_eq!(restored.tabs.focused_windows().split_dir, SplitDir::Horizontal);
    }

    #[test]
    fn restore_command_history_caps_length() {
        let mut app = App::from_path(None).unwrap();
        let history = (0..120).map(|idx| format!("cmd-{idx}")).collect::<Vec<_>>();
        let state = SessionState {
            version: SESSION_VERSION,
            buffers: Vec::new(),
            tabs: Vec::new(),
            focused_tab: 0,
            marks: Vec::new(),
            jump_list: Vec::new(),
            jump_list_idx: 0,
            command_history: history,
        };

        state.restore(&mut app).unwrap();

        assert_eq!(app.command_history().len(), 100);
        assert_eq!(app.command_history().first().unwrap(), "cmd-20");
        assert_eq!(app.command_history().last().unwrap(), "cmd-119");
    }
}
