use super::App;

pub(super) struct FoldVerticalMotion {
    pub(super) line_count: usize,
    pub(super) up: bool,
    pub(super) modify_selection: bool,
}

impl App {
    pub(super) fn fold_vertical_motion(
        &self,
        method: &str,
        visible_count: usize,
    ) -> Option<FoldVerticalMotion> {
        let (up, modify_selection) = match method {
            "move_up" => (true, false),
            "move_up_and_modify_selection" => (true, true),
            "move_down" => (false, false),
            "move_down_and_modify_selection" => (false, true),
            _ => return None,
        };

        let buffer = self.backend.active();
        if self.folds.get(buffer.id).is_empty() {
            return None;
        }

        let target = self.folds.line_after_visible_steps(
            buffer.id,
            buffer.cursor_line,
            !up,
            visible_count,
            buffer.line_count(),
        );
        let line_count = target.abs_diff(buffer.cursor_line);
        if line_count == visible_count {
            return None;
        }

        Some(FoldVerticalMotion { line_count, up, modify_selection })
    }
}
