//! `impl App` methods: terminal domain.
use super::*;

impl App {
    pub(super) fn open_generated_buffer(&mut self, title: &str, body: &str) {
        match self.backend.open_named_scratch_buffer(title) {
            Ok(buf_id) => {
                let _ = self.backend.switch_to_id(buf_id);
                self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                self.viewport = Viewport::default();
                if !body.is_empty() {
                    let _ = self.backend.send_edit("insert", json!({ "chars": body }));
                }
                self.backend.status_message = Some(title.to_owned());
            }
            Err(err) => {
                self.backend.status_message = Some(format!("{title} failed: {err}"));
            }
        }
    }
    pub(super) fn run_terminal_command(&mut self, command: crate::terminal::TerminalCommand) {
        let cwd = self.shell_working_dir();

        match crate::terminal::run_command(&command, &cwd) {
            Ok(result) => {
                let title = command.title.clone();
                let body = crate::terminal::render_transcript(&result);
                self.open_generated_buffer(&title, &body);
                self.backend.status_message = Some(if result.success {
                    format!("{} finished", title)
                } else {
                    format!("{} failed", title)
                });
            }
            Err(err) => {
                self.backend.status_message = Some(format!("shell failed: {err}"));
            }
        }
    }
}
