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
use crate::lsp::DiagnosticsStore;
use crate::ui::hover_popup::HoverQuery;
use crate::ui::scroll;
use crate::ui::symbols;
use crate::ui::text::{
    display_width, expand_tabs_in_spans, highlight_color, mark_range, truncate_spans_to_width,
};
use lsp_types::DiagnosticSeverity;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::path::{Path, PathBuf};

const LINE_NUMBER_WIDTH: usize = 5;
const STATUS_BAR_HEIGHT: u16 = 1;
const HINTS: &str = "j/k move  C-d/C-u half-page  gg/G top/bottom  ]d/[d diag  K hover  gd def  gr refs  C-o/C-t jump  Tab symbol  q quit";

/// State for one open file. Construction does the (comparatively) expensive
/// work — reading line boundaries and running the whole-file highlight pass
/// once — so scrolling and rendering afterward touch nothing but indices.
pub struct FileView {
    pub display_path: String,
    /// Absolute path to the file on disk, and the boundary
    /// [`crate::lsp::adapter::workspace_root`] searches within — `None`
    /// when the source came from somewhere `Action::Hover` can't target
    /// (currently unused by any caller, but kept optional rather than
    /// requiring every `FileView` construction site to fabricate a path,
    /// e.g. for a future "preview" viewer over unsaved content).
    file_path: Option<PathBuf>,
    git_root: PathBuf,
    highlighter: FileHighlighter,
    /// Set when this file's name is lockfile-ish or its line count exceeds
    /// `[ui] highlight_max_lines` — mirrors `DiffFile::skip_highlighting`
    /// for the single-file case (see that method's docs). `highlighter`
    /// above was already built with [`Language::Plain`] when this is `true`
    /// (tree-sitter never ran at all, not just "ran and got discarded"),
    /// and [`Self::file_path`] is excluded from LSP warm-up's `didOpen` —
    /// see `ui::warm_up_root`'s docs on why that shares this same
    /// threshold. `render_status` shows a note when this is set.
    pub highlight_skipped: bool,
    pub cursor: usize,
    pub scroll_offset: usize,
    /// Index into the current line's [`symbols::scan`] output — mirrors
    /// [`crate::ui::app::App::active_symbol`]; see that field's docs.
    pub active_symbol: usize,
    pub pending_keys: String,
    pub should_quit: bool,
    viewport_height: usize,
}

impl FileView {
    /// Builds a view over `source`, optionally recording `target` — the
    /// absolute path on disk and its git root — so `Action::Hover` has
    /// somewhere to send a request. `ktmr open` always has a real file and
    /// passes `Some`; tests that only exercise scrolling/highlighting pass
    /// `None`.
    pub fn with_hover_target(
        display_path: String,
        source: &str,
        target: Option<(PathBuf, PathBuf)>,
    ) -> Self {
        let highlight_skipped = crate::diff::is_lockfile_ish(&display_path)
            || source.lines().count() > crate::config::highlight_max_lines();
        let language = if highlight_skipped {
            Language::Plain
        } else {
            Language::detect(&display_path)
        };
        let (file_path, git_root) = match target {
            Some((file, root)) => (Some(file), root),
            None => (None, PathBuf::new()),
        };
        Self {
            highlighter: FileHighlighter::new(language, source),
            display_path,
            file_path,
            git_root,
            highlight_skipped,
            cursor: 0,
            scroll_offset: 0,
            active_symbol: 0,
            pending_keys: String::new(),
            should_quit: false,
            viewport_height: 1,
        }
    }

    /// The absolute path this view is showing, if it has a hover target —
    /// `None` for a view constructed with [`Self::with_hover_target`]'s
    /// `target: None` (a preview over content with nowhere on disk to point
    /// at). Used by [`crate::ui::navigation`] to recognize "the jump target
    /// is the file already open in this view" and by the diagnostics gutter
    /// to look up this file's entries.
    pub fn file_path(&self) -> Option<&Path> {
        self.file_path.as_deref()
    }

    pub fn git_root(&self) -> &Path {
        &self.git_root
    }

    /// The plain text of the line the cursor currently sits on — used for
    /// symbol scanning within this module and, via
    /// [`crate::ui::navigation`], to resolve the display column of the
    /// active symbol when recording this position on the jump stack.
    pub fn cursor_line_text(&self) -> String {
        self.highlighter
            .line(self.cursor)
            .iter()
            .map(|s| s.text.as_str())
            .collect()
    }

    fn cycle_symbol(&mut self, delta: isize) {
        let text = self.cursor_line_text();
        let symbols = symbols::scan(&text);
        if symbols.is_empty() {
            return;
        }
        let len = symbols.len() as isize;
        let next = (self.active_symbol as isize + delta).rem_euclid(len);
        self.active_symbol = next as usize;
    }

    /// As [`crate::ui::app::App::hover_query`]: what `Action::Hover` should
    /// ask about, or `None` when this view has no hover target (see
    /// `file_path`) or the current line has no identifier-like token.
    pub fn hover_query(&self) -> Option<HoverQuery> {
        let file = self.file_path.clone()?;
        let line_text = self.cursor_line_text();
        let symbol = symbols::scan(&line_text).get(self.active_symbol).copied()?;
        Some(HoverQuery {
            file,
            git_root: self.git_root.clone(),
            line: self.cursor as u32,
            line_text,
            display_col: symbol.display_start,
        })
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
        let cursor_before = self.cursor;
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
            Action::NextSymbol => self.cycle_symbol(1),
            Action::PrevSymbol => self.cycle_symbol(-1),
            Action::Quit => self.should_quit = true,
            // `ui::mod` intercepts all of these before they reach here,
            // same as in `App::update` — see that method's comment.
            Action::Hover
            | Action::Cancel
            | Action::GotoDefinition
            | Action::FindReferences
            | Action::NextDiagnostic
            | Action::PrevDiagnostic
            | Action::JumpBack
            | Action::JumpForward
            | Action::Confirm => {}
            // A single file has no hunks, no other files, no sidebar, and
            // only one column — these diff-view actions are no-ops here
            // rather than unreachable, so the shared keymap doesn't need a
            // per-view binding table. `ToggleTimeline`/`ToggleRangeSelect`
            // are the same story: the timeline only relates to the root
            // diff, and `ui::mod` never even offers `t` a `TimelineView` to
            // push when a `FileView` is on top (see `handle_action`).
            Action::NextHunk
            | Action::PrevHunk
            | Action::NextFile
            | Action::PrevFile
            | Action::ToggleSidebar
            | Action::ToggleLayout
            | Action::ToggleTimeline
            | Action::ToggleRangeSelect
            | Action::AddComment
            | Action::ToggleComments => {}
        }
        if self.cursor != cursor_before {
            self.active_symbol = 0;
        }
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        self.scroll_offset =
            scroll::clamp_scroll(self.cursor, self.viewport_height, self.scroll_offset);
    }

    /// Moves the cursor to `line` and, if the active symbol there covers
    /// `display_col`, selects it — mirrors [`crate::ui::app::App::jump_cursor_to`];
    /// see that method's docs for why centering rather than clamping.
    pub fn jump_cursor_to(&mut self, line: usize, display_col: usize) {
        self.cursor = line.min(self.line_count().saturating_sub(1));
        let text = self.cursor_line_text();
        self.active_symbol = symbols::scan(&text)
            .iter()
            .position(|s| s.display_start <= display_col && display_col < s.display_end)
            .unwrap_or(0);
        self.scroll_offset = scroll::center(self.cursor, self.viewport_height);
        self.clamp_scroll();
    }

    /// As [`crate::ui::app::App::jump_to_diagnostic`], for this single-file
    /// view: here a "row" already *is* a 0-based line number, so no
    /// `lsp_target`-style translation is needed first.
    pub fn jump_to_diagnostic(&mut self, diagnostics: &DiagnosticsStore, forward: bool) {
        let Some(file) = self.file_path.as_deref() else {
            return;
        };
        let lines = diagnostics.lines_with_diagnostics(file);
        if lines.is_empty() {
            return;
        }
        let current = self.cursor as u32;
        let target = if forward {
            lines
                .iter()
                .find(|&&l| l > current)
                .or_else(|| lines.first())
        } else {
            lines
                .iter()
                .rev()
                .find(|&&l| l < current)
                .or_else(|| lines.last())
        };
        if let Some(&line) = target {
            self.cursor = (line as usize).min(self.line_count().saturating_sub(1));
            self.active_symbol = 0;
            self.scroll_offset = scroll::center(self.cursor, self.viewport_height);
            self.clamp_scroll();
        }
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

pub fn render(frame: &mut Frame, area: Rect, view: &FileView, diagnostics: &DiagnosticsStore) {
    let block = Block::default().borders(Borders::LEFT).title(" file ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width as usize;
    let visible = (view.scroll_offset..view.line_count()).take(inner.height as usize);

    let lines: Vec<Line> = visible
        .map(|idx| content_line(view, idx, idx == view.cursor, width, diagnostics))
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

/// As `diff_view`'s glyph of the same name — see that module's docs. Kept
/// as a separate small copy rather than a shared helper because the two
/// views build their gutter strings differently enough (add/del marker and
/// dual line numbers here vs. a single line number there) that threading a
/// shared function through both would need as many parameters as it saved
/// lines.
fn diagnostic_glyph_span(severity: Option<DiagnosticSeverity>) -> Span<'static> {
    let (glyph, color) = match severity {
        Some(DiagnosticSeverity::ERROR) => ("\u{25CF}", Color::Red),
        Some(DiagnosticSeverity::WARNING) => ("\u{25B2}", Color::Yellow),
        Some(_) => ("\u{00B7}", Color::Blue),
        None => (" ", Color::Reset),
    };
    Span::styled(format!("{glyph} "), Style::default().fg(color))
}

fn content_line(
    view: &FileView,
    idx: usize,
    is_cursor: bool,
    width: usize,
    diagnostics: &DiagnosticsStore,
) -> Line<'static> {
    let severity = view
        .file_path()
        .and_then(|file| diagnostics.severity_at(file, idx as u32));
    let diagnostic_span = diagnostic_glyph_span(severity);

    let gutter = format!("{:>LINE_NUMBER_WIDTH$} \u{2502} ", idx + 1);
    let gutter_width = display_width(&gutter) + display_width(diagnostic_span.content.as_ref());
    let content_width = width.saturating_sub(gutter_width);

    let spans = expand_tabs_in_spans(
        view.highlighter.line(idx).to_vec(),
        crate::config::tab_width(),
    );
    let spans = truncate_spans_to_width(&spans, content_width);

    let mut content_spans: Vec<Span<'static>> = spans
        .into_iter()
        .map(|span| Span::styled(span.text, Style::default().fg(highlight_color(span.kind))))
        .collect();
    if is_cursor
        && let Some(active) = symbols::scan(&view.cursor_line_text()).get(view.active_symbol)
    {
        content_spans = mark_range(
            content_spans,
            active.display_start,
            active.display_end,
            Style::default().add_modifier(Modifier::UNDERLINED),
        );
    }

    let mut line_spans = vec![
        diagnostic_span,
        Span::styled(gutter, Style::default().fg(Color::DarkGray)),
    ];
    line_spans.extend(content_spans);

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

pub fn render_status(frame: &mut Frame, area: Rect, view: &FileView, status_note: Option<&str>) {
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

    if view.highlight_skipped {
        spans.push(Span::styled(
            "· highlight off (large file) ",
            Style::default().fg(Color::DarkGray),
        ));
    }

    if let Some(note) = status_note {
        spans.push(Span::styled(
            format!("· {note} "),
            Style::default()
                .fg(Color::Cyan)
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
        let mut view = FileView::with_hover_target("src/main.rs".to_owned(), source, None);
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

    #[test]
    fn plain_new_has_no_hover_target() {
        let view = test_view();
        assert_eq!(view.hover_query(), None);
    }

    #[test]
    fn next_symbol_cycles_and_resets_on_cursor_move() {
        let mut view = test_view();
        // Line 0 is "fn main() {" — two symbols: "fn", "main".
        assert_eq!(view.active_symbol, 0);
        view.update(Action::NextSymbol);
        assert_eq!(view.active_symbol, 1);
        view.update(Action::NextSymbol);
        assert_eq!(view.active_symbol, 0, "wraps back to the first symbol");
        view.update(Action::CursorDown);
        assert_eq!(
            view.active_symbol, 0,
            "moving the cursor resets the active symbol"
        );
    }

    #[test]
    fn hover_query_targets_the_active_symbol_when_a_hover_target_is_configured() {
        let source = "fn main() {\n    let x = 1;\n}\n";
        let mut view = FileView::with_hover_target(
            "src/main.rs".to_owned(),
            source,
            Some((PathBuf::from("/repo/src/main.rs"), PathBuf::from("/repo"))),
        );
        view.set_viewport_height(2);

        let query = view.hover_query().expect("first line has symbols");
        assert_eq!(query.file, PathBuf::from("/repo/src/main.rs"));
        assert_eq!(query.line, 0);
        assert_eq!(query.line_text, "fn main() {");
        assert_eq!(query.display_col, 0); // "fn"

        view.update(Action::NextSymbol);
        let query = view.hover_query().expect("still line 0");
        assert_eq!(query.display_col, 3); // "main"
    }
}
