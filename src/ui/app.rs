//! Application state and its pure state transitions. Deliberately free of
//! any terminal or ratatui dependency: `App` can be constructed and driven
//! entirely with parsed diff data and [`Action`]s, which is what makes it
//! testable without a real terminal.

use crate::diff::{DiffFile, RenderRow, flatten};
use crate::keymap::Action;

/// All state the UI needs to render a frame and respond to input.
pub struct App {
    pub repo_name: String,
    pub files: Vec<DiffFile>,
    pub rows: Vec<RenderRow>,
    pub cursor: usize,
    pub scroll_offset: usize,
    pub sidebar_visible: bool,
    pub pending_keys: String,
    pub should_quit: bool,
    viewport_height: usize,
}

impl App {
    pub fn new(repo_name: String, files: Vec<DiffFile>) -> Self {
        let rows = flatten(&files);
        Self {
            repo_name,
            files,
            rows,
            cursor: 0,
            scroll_offset: 0,
            sidebar_visible: true,
            pending_keys: String::new(),
            should_quit: false,
            viewport_height: 1,
        }
    }

    /// The diff pane's visible row count changes on terminal resize; the
    /// event loop reports it before each frame so half-page scrolling and
    /// scroll-to-cursor clamping use an up-to-date value.
    pub fn set_viewport_height(&mut self, height: usize) {
        self.viewport_height = height.max(1);
        self.clamp_scroll();
    }

    /// Index into `files`/`rows` for whichever file the cursor currently
    /// sits within, for sidebar highlighting.
    pub fn selected_file(&self) -> usize {
        match self.rows.get(self.cursor) {
            Some(RenderRow::FileHeader { file_idx }) => *file_idx,
            Some(RenderRow::HunkHeader { file_idx, .. }) => *file_idx,
            Some(RenderRow::Line { file_idx, .. }) => *file_idx,
            None => 0,
        }
    }

    pub fn update(&mut self, action: Action) {
        if self.rows.is_empty() {
            self.should_quit |= action == Action::Quit;
            return;
        }
        let last = self.rows.len() - 1;
        match action {
            Action::CursorDown => self.cursor = (self.cursor + 1).min(last),
            Action::CursorUp => self.cursor = self.cursor.saturating_sub(1),
            Action::HalfPageDown => {
                self.cursor = (self.cursor + self.half_page()).min(last);
            }
            Action::HalfPageUp => {
                self.cursor = self.cursor.saturating_sub(self.half_page());
            }
            Action::Top => self.cursor = 0,
            Action::Bottom => self.cursor = last,
            Action::NextHunk => self.jump_to(|row| matches!(row, RenderRow::HunkHeader { .. })),
            Action::PrevHunk => {
                self.jump_to_prev(|row| matches!(row, RenderRow::HunkHeader { .. }))
            }
            Action::NextFile => self.jump_to(|row| matches!(row, RenderRow::FileHeader { .. })),
            Action::PrevFile => {
                self.jump_to_prev(|row| matches!(row, RenderRow::FileHeader { .. }))
            }
            Action::ToggleSidebar => self.sidebar_visible = !self.sidebar_visible,
            Action::Quit => self.should_quit = true,
        }
        self.clamp_scroll();
    }

    fn half_page(&self) -> usize {
        (self.viewport_height / 2).max(1)
    }

    fn jump_to(&mut self, is_target: impl Fn(&RenderRow) -> bool) {
        if let Some(pos) = self
            .rows
            .iter()
            .enumerate()
            .skip(self.cursor + 1)
            .find(|(_, r)| is_target(r))
            .map(|(i, _)| i)
        {
            self.cursor = pos;
        }
    }

    fn jump_to_prev(&mut self, is_target: impl Fn(&RenderRow) -> bool) {
        if let Some(pos) = self.rows[..self.cursor]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, r)| is_target(r))
            .map(|(i, _)| i)
        {
            self.cursor = pos;
        }
    }

    fn clamp_scroll(&mut self) {
        if self.cursor < self.scroll_offset {
            self.scroll_offset = self.cursor;
        } else if self.cursor >= self.scroll_offset + self.viewport_height {
            self.scroll_offset = self.cursor + 1 - self.viewport_height;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_unified_diff;

    const FIXTURE: &str = include_str!("../diff/fixtures/multi_file.diff");

    fn test_app() -> App {
        let files = parse_unified_diff(FIXTURE);
        let mut app = App::new("test-repo".to_owned(), files);
        app.set_viewport_height(10);
        app
    }

    #[test]
    fn cursor_down_and_up_stay_in_bounds() {
        let mut app = test_app();
        app.update(Action::CursorUp);
        assert_eq!(app.cursor, 0, "cannot go above the first row");
        app.update(Action::Bottom);
        let last = app.rows.len() - 1;
        app.update(Action::CursorDown);
        assert_eq!(app.cursor, last, "cannot go below the last row");
    }

    #[test]
    fn top_and_bottom_jump_to_extremes() {
        let mut app = test_app();
        app.update(Action::Bottom);
        assert_eq!(app.cursor, app.rows.len() - 1);
        app.update(Action::Top);
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn next_file_visits_each_file_header_in_order() {
        let mut app = test_app();
        let mut visited_files = Vec::new();
        loop {
            let before = app.cursor;
            app.update(Action::NextFile);
            if app.cursor == before {
                break; // no more files ahead; NextFile is a no-op at the last file
            }
            match app.rows[app.cursor] {
                RenderRow::FileHeader { file_idx } => visited_files.push(file_idx),
                _ => break,
            }
        }
        assert_eq!(visited_files, vec![1, 2, 3, 4]);
    }

    #[test]
    fn toggle_sidebar_flips_visibility() {
        let mut app = test_app();
        assert!(app.sidebar_visible);
        app.update(Action::ToggleSidebar);
        assert!(!app.sidebar_visible);
    }

    #[test]
    fn quit_sets_should_quit() {
        let mut app = test_app();
        assert!(!app.should_quit);
        app.update(Action::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn scroll_follows_cursor_past_viewport_bottom() {
        let mut app = test_app();
        app.set_viewport_height(3);
        app.update(Action::Bottom);
        assert!(app.cursor >= app.scroll_offset);
        assert!(app.cursor < app.scroll_offset + 3);
    }
}
