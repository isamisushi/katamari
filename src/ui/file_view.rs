//! A read-only, full-file viewer: line numbers, whole-file syntax
//! highlighting, and the same scrolling keys the diff view uses. `ktmr open`
//! launches one standalone, but the type is deliberately free of any
//! assumption that it's the only view running — it exists so a later
//! milestone's "go to definition" can push a `FileView` onto the
//! [`crate::ui::ViewStack`] on top of whatever the user was already looking
//! at, the same way this module's own `render`/`update` are used today.
//!
//! Like [`crate::ui::app::App`], this holds no terminal or ratatui state
//! beyond what a frame needs to draw — `update` is a pure state transition,
//! testable without a real terminal.

use crate::highlight::{FileHighlighter, Language};
use crate::keymap::Action;
use crate::ui::scroll;
use crate::ui::text::{display_width, highlight_color, truncate_spans_to_width};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

const LINE_NUMBER_WIDTH: usize = 5;
const STATUS_BAR_HEIGHT: u16 = 1;
const HINTS: &str = "j/k move  C-d/C-u half-page  gg/G top/bottom  q quit";

/// State for one open file. Construction does the (comparatively) expensive
/// work — reading line boundaries and running the whole-file highlight pass
/// once — so scrolling and rendering afterward touch nothing but indices.
pub struct FileView {
    pub display_path: String,
    highlighter: FileHighlighter,
    pub cursor: usize,
    pub scroll_offset: usize,
    pub pending_keys: String,
    pub should_quit: bool,
    viewport_height: usize,
}

impl FileView {
    pub fn new(display_path: String, source: &str) -> Self {
        let language = Language::detect(&display_path);
        Self {
            highlighter: FileHighlighter::new(language, source),
            display_path,
            cursor: 0,
            scroll_offset: 0,
            pending_keys: String::new(),
            should_quit: false,
            viewport_height: 1,
        }
    }

    /// The number of lines in the opened file — the file view's analogue of
    /// `App::rows.len()`.
    pub fn line_count(&self) -> usize {
        self.highlighter.line_count()
    }

    /// The content pane's visible row count changes on terminal resize; the
    /// event loop reports it before each frame, mirroring `App`.
    pub fn set_viewport_height(&mut self, height: usize) {
        self.viewport_height = height.max(1);
        self.clamp_scroll();
    }

    pub fn update(&mut self, action: Action) {
        if self.line_count() == 0 {
            self.should_quit |= action == Action::Quit;
            return;
        }
        let last = self.line_count() - 1;
        match action {
            Action::CursorDown => self.cursor = (self.cursor + 1).min(last),
            Action::CursorUp => self.cursor = self.cursor.saturating_sub(1),
            Action::HalfPageDown => {
                self.cursor = (self.cursor + scroll::half_page(self.viewport_height)).min(last);
            }
            Action::HalfPageUp => {
                self.cursor = self
                    .cursor
                    .saturating_sub(scroll::half_page(self.viewport_height));
            }
            Action::Top => self.cursor = 0,
            Action::Bottom => self.cursor = last,
            Action::Quit => self.should_quit = true,
            // A single file has no hunks, no other files, no sidebar, and
            // only one column — these diff-view actions are no-ops here
            // rather than unreachable, so the shared keymap doesn't need a
            // per-view binding table.
            Action::NextHunk
            | Action::PrevHunk
            | Action::NextFile
            | Action::PrevFile
            | Action::ToggleSidebar
            | Action::ToggleLayout => {}
        }
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        self.scroll_offset =
            scroll::clamp_scroll(self.cursor, self.viewport_height, self.scroll_offset);
    }
}

pub struct Areas {
    pub content: Rect,
    pub status: Rect,
}

pub fn layout(area: Rect) -> Areas {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(STATUS_BAR_HEIGHT)])
        .split(area);
    Areas {
        content: rows[0],
        status: rows[1],
    }
}

pub fn render(frame: &mut Frame, area: Rect, view: &FileView) {
    let block = Block::default().borders(Borders::LEFT).title(" file ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width as usize;
    let visible = (view.scroll_offset..view.line_count()).take(inner.height as usize);

    let lines: Vec<Line> = visible
        .map(|idx| content_line(view, idx, idx == view.cursor, width))
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn content_line(view: &FileView, idx: usize, is_cursor: bool, width: usize) -> Line<'static> {
    let gutter = format!("{:>LINE_NUMBER_WIDTH$} \u{2502} ", idx + 1);
    let gutter_width = display_width(&gutter);
    let content_width = width.saturating_sub(gutter_width);

    let spans = truncate_spans_to_width(view.highlighter.line(idx), content_width);

    let mut line_spans = vec![Span::styled(gutter, Style::default().fg(Color::DarkGray))];
    for span in spans {
        line_spans.push(Span::styled(
            span.text,
            Style::default().fg(highlight_color(span.kind)),
        ));
    }

    let rendered_width: usize = line_spans.iter().map(ratatui::text::Span::width).sum();
    if rendered_width < width {
        line_spans.push(Span::raw(" ".repeat(width - rendered_width)));
    }

    let mut line = Line::from(line_spans);
    if is_cursor {
        line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
    }
    line
}

pub fn render_status(frame: &mut Frame, area: Rect, view: &FileView) {
    let position = format!("{}/{}", view.cursor + 1, view.line_count().max(1));
    let mut spans = vec![
        Span::styled(
            format!(" {} ", view.display_path),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("· {position} ")),
    ];

    if !view.pending_keys.is_empty() {
        spans.push(Span::styled(
            format!("· {} ", view.pending_keys),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    spans.push(Span::styled(
        format!("· {HINTS}"),
        Style::default().fg(Color::DarkGray),
    ));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_view() -> FileView {
        let source = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        let mut view = FileView::new("src/main.rs".to_owned(), source);
        view.set_viewport_height(2);
        view
    }

    #[test]
    fn cursor_down_and_up_stay_in_bounds() {
        let mut view = test_view();
        view.update(Action::CursorUp);
        assert_eq!(view.cursor, 0, "cannot go above the first line");
        view.update(Action::Bottom);
        let last = view.cursor;
        view.update(Action::CursorDown);
        assert_eq!(view.cursor, last, "cannot go below the last line");
    }

    #[test]
    fn top_and_bottom_jump_to_extremes() {
        let mut view = test_view();
        view.update(Action::Bottom);
        assert_eq!(view.cursor, 3);
        view.update(Action::Top);
        assert_eq!(view.cursor, 0);
    }

    #[test]
    fn quit_sets_should_quit() {
        let mut view = test_view();
        assert!(!view.should_quit);
        view.update(Action::Quit);
        assert!(view.should_quit);
    }

    #[test]
    fn diff_only_actions_are_no_ops() {
        let mut view = test_view();
        view.update(Action::NextHunk);
        assert_eq!(view.cursor, 0);
        view.update(Action::ToggleSidebar);
        assert_eq!(view.cursor, 0);
    }

    #[test]
    fn scroll_follows_cursor_past_viewport_bottom() {
        let mut view = test_view();
        view.update(Action::Bottom);
        assert!(view.cursor >= view.scroll_offset);
        assert!(view.cursor < view.scroll_offset + 2);
    }
}
