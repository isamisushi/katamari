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

use crate::highlight::{FileHighlighter, Language, Span as HlSpan};
use crate::keymap::Action;
use crate::lsp::DiagnosticsStore;
use crate::ui::hints::{self, HintItem};
use crate::ui::hover_popup::HoverQuery;
use crate::ui::scroll;
use crate::ui::symbols;
use crate::ui::text::{
    display_width, expand_tabs_in_spans, highlight_color, mark_range, truncate_spans_to_width,
    wrap_spans_to_width, wrapped_row_count,
};
use lsp_types::DiagnosticSeverity;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::path::{Path, PathBuf};

const LINE_NUMBER_WIDTH: usize = 5;

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
    /// This pane's per-row wrap width — see [`crate::ui::app::App`]'s field
    /// of the same purpose for why it's refreshed every frame rather than
    /// computed once, and [`content_width_for_pane`] for how a pane width
    /// becomes this.
    content_width: usize,
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
            content_width: usize::MAX, // see `App::content_width`'s docs
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

    /// As [`crate::ui::app::App::set_content_width`] — refreshed every
    /// frame alongside [`Self::set_viewport_height`] so [`Self::row_visual_height`]'s
    /// wrap width stays current with the pane's actual size.
    pub fn set_content_width(&mut self, width: usize) {
        self.content_width = width.max(1);
        self.clamp_scroll();
    }

    /// As [`crate::ui::app::App::row_visual_height`]: `1` when `[ui] wrap`
    /// is off, otherwise however many visual rows line `idx`'s highlighted
    /// text soft-wraps into at [`Self::content_width`].
    fn row_visual_height(&self, idx: usize) -> usize {
        if !crate::config::wrap_enabled() {
            return 1;
        }
        let text: String = self
            .highlighter
            .line(idx)
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        wrapped_row_count(&text, crate::config::tab_width(), self.content_width)
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
                self.cursor =
                    scroll::half_page_down(self.cursor, last, self.viewport_height, |i| {
                        self.row_visual_height(i)
                    });
            }
            Action::HalfPageUp => {
                self.cursor = scroll::half_page_up(self.cursor, self.viewport_height, |i| {
                    self.row_visual_height(i)
                });
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
            // per-view binding table. `ToggleTimeline`/`ToggleLogView`/
            // `ToggleRangeSelect`/`OpenScopeMenu` are the same story: the
            // timeline/log/scope-menu popup only relate to the root diff,
            // and `ui::mod` never even offers `t`/`L`/`o` a view to push or
            // a diff to swap when a `FileView` is on top (see
            // `handle_action`). `ExpandFold`/`CollapseFold` too: an opened
            // file is read whole (no hunks means no gaps — see
            // `crate::diff::file_gaps`), so there is never a fold row here
            // to act on. `OpenSearch`/`NextMatch`/`PrevMatch` join the same
            // bucket for the same reason as the rest of it: Issue #5's `/`
            // search is a deliberately diff-view-only feature (see
            // `ui::mod::handle_action`'s own `View::Diff`-only gate for
            // these three), so a single opened file — which has no other
            // files, hunks, or fold rows to search *across* the way a diff
            // does — never gets a prompt of its own here either.
            Action::NextHunk
            | Action::PrevHunk
            | Action::NextFile
            | Action::PrevFile
            | Action::ToggleSidebar
            | Action::ToggleLayout
            | Action::ToggleTimeline
            | Action::ToggleLogView
            | Action::ToggleUnits
            | Action::RegenerateUnits
            | Action::ToggleHints
            | Action::ToggleLspInspector
            | Action::ToggleRangeSelect
            | Action::OpenScopeMenu
            | Action::AddComment
            | Action::ToggleComments
            | Action::ExpandFold
            | Action::CollapseFold
            | Action::OpenSearch
            | Action::NextMatch
            | Action::PrevMatch => {}
            // `ui::mod` intercepts this before it reaches here, same as
            // every other action in the `Hover`/`Cancel`/... group above —
            // opening `ui::help`'s popup needs the live `Keymap`, which
            // `FileView` doesn't own, and it opens from any view, so it's a
            // no-op here rather than something this view ever handles
            // itself. See `Action::OpenHelp`'s docs.
            Action::OpenHelp => {}
        }
        if self.cursor != cursor_before {
            self.active_symbol = 0;
        }
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        self.scroll_offset =
            scroll::clamp_scroll(self.cursor, self.viewport_height, self.scroll_offset, |i| {
                self.row_visual_height(i)
            });
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
        self.scroll_offset = scroll::center(self.cursor, self.viewport_height, |i| {
            self.row_visual_height(i)
        });
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
            self.scroll_offset = scroll::center(self.cursor, self.viewport_height, |i| {
                self.row_visual_height(i)
            });
            self.clamp_scroll();
        }
    }
}

pub struct Areas {
    pub content: Rect,
    pub status: Rect,
}

/// `status_height` is [`hints::required_height`] applied to
/// [`hints::file_view_items`] and `area`'s width — computed by the caller
/// (see `ui::mod`'s draw/event-loop functions) so the same value both sizes
/// this split and, later, the number of rows `render_status` actually
/// renders into it.
pub fn layout(area: Rect, status_height: u16) -> Areas {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(status_height)])
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
    let viewport_height = inner.height as usize;
    let wrap = crate::config::wrap_enabled();

    let mut lines: Vec<Line> = Vec::with_capacity(viewport_height);
    let mut idx = view.scroll_offset;
    while lines.len() < viewport_height && idx < view.line_count() {
        for line in content_line(view, idx, idx == view.cursor, width, diagnostics, wrap) {
            if lines.len() >= viewport_height {
                break;
            }
            lines.push(line);
        }
        idx += 1;
    }

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

/// The exact display-column width every content row's gutter (diagnostic
/// glyph + line number + separator) occupies, regardless of that row's
/// actual line number or diagnostic state — see `diff_view::gutter_width`'s
/// identical "always reserve the width" rationale, which this mirrors for
/// the file view's simpler (single line-number, no add/del marker) gutter.
fn gutter_width() -> usize {
    const DIAG_WIDTH: usize = 2; // glyph + trailing space, see `diagnostic_glyph_span`
    LINE_NUMBER_WIDTH + 1 + 1 + 1 + DIAG_WIDTH // number + space + │ + space
}

/// The display-column budget available to a content row's highlighted text
/// at a pane of `pane_width` columns: the pane's border (1 column, see
/// `render`'s `Borders::LEFT`) and [`gutter_width`] subtracted out. The
/// single source both [`content_line`]'s own wrap and
/// [`FileView::row_visual_height`]'s scroll-math lookups derive a line's
/// wrap width from, so they never disagree about how wide a line is
/// allowed to be before it needs a continuation row.
pub fn content_width_for_pane(pane_width: u16) -> usize {
    (pane_width.saturating_sub(1) as usize).saturating_sub(gutter_width())
}

/// The gutter for a wrapped line's second-and-later visual rows: blank
/// where the diagnostic glyph and line number would be (so a continuation
/// row is never mistaken for a diagnostic-bearing line of its own or a
/// separate numbered line), with a `↪` marking it as a continuation of the
/// row above — the same width as [`gutter_width`] so the highlighted text
/// after it lines up in the same column regardless of which visual row of
/// its logical line it belongs to.
fn continuation_gutter() -> Vec<Span<'static>> {
    vec![
        Span::raw(" ".repeat(2)), // diagnostic glyph's reserved width
        Span::styled(
            format!("{:>LINE_NUMBER_WIDTH$} \u{21aa} ", ""),
            Style::default().fg(Color::DarkGray),
        ),
    ]
}

fn content_line(
    view: &FileView,
    idx: usize,
    is_cursor: bool,
    width: usize,
    diagnostics: &DiagnosticsStore,
    wrap: bool,
) -> Vec<Line<'static>> {
    let severity = view
        .file_path()
        .and_then(|file| diagnostics.severity_at(file, idx as u32));
    let gutter = format!("{:>LINE_NUMBER_WIDTH$} \u{2502} ", idx + 1);
    let content_width = width.saturating_sub(gutter_width());

    let spans = expand_tabs_in_spans(
        view.highlighter.line(idx).to_vec(),
        crate::config::tab_width(),
    );
    let visual_rows: Vec<Vec<HlSpan>> = if wrap {
        wrap_spans_to_width(&spans, content_width)
    } else {
        vec![truncate_spans_to_width(&spans, content_width)]
    };

    // Resolved once per logical line (not per visual row): the active
    // symbol's display-column range is in terms of the whole logical
    // line, and gets clipped down to whichever visual row(s) it actually
    // falls within below.
    let active_symbol = is_cursor
        .then(|| {
            symbols::scan(&view.cursor_line_text())
                .get(view.active_symbol)
                .copied()
        })
        .flatten();

    let mut out = Vec::with_capacity(visual_rows.len());
    let mut col_offset = 0usize;
    for (i, row_spans) in visual_rows.into_iter().enumerate() {
        let row_width: usize = row_spans.iter().map(|s| display_width(&s.text)).sum();
        let mut content_spans: Vec<Span<'static>> = row_spans
            .into_iter()
            .map(|span| Span::styled(span.text, Style::default().fg(highlight_color(span.kind))))
            .collect();
        if let Some(active) = active_symbol {
            let (start, end) = (active.display_start, active.display_end);
            if end > col_offset && start < col_offset + row_width {
                let local_start = start.saturating_sub(col_offset);
                let local_end = (end - col_offset).min(row_width);
                content_spans = mark_range(
                    content_spans,
                    local_start,
                    local_end,
                    Style::default().add_modifier(Modifier::UNDERLINED),
                );
            }
        }

        let mut line_spans = if i == 0 {
            vec![
                diagnostic_glyph_span(severity),
                Span::styled(gutter.clone(), Style::default().fg(Color::DarkGray)),
            ]
        } else {
            continuation_gutter()
        };
        line_spans.extend(content_spans);

        let rendered_width: usize = line_spans.iter().map(ratatui::text::Span::width).sum();
        if rendered_width < width {
            line_spans.push(Span::raw(" ".repeat(width - rendered_width)));
        }

        let mut line = Line::from(line_spans);
        if is_cursor {
            line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
        }
        out.push(line);
        col_offset += row_width;
    }
    out
}

/// `hint_items` is [`hints::file_view_items`] read off the session's active
/// keymap — see `status_bar::render`'s docs on why it's built by the caller
/// rather than here.
pub fn render_status(
    frame: &mut Frame,
    area: Rect,
    view: &FileView,
    status_note: Option<&str>,
    hint_items: &[HintItem],
) {
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

    let wrapped = hints::wrap_for_area(hint_items, area.width);
    let mut lines = vec![Line::from(spans)];
    lines.extend(hints::render_lines(&wrapped));
    frame.render_widget(Paragraph::new(lines), area);
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
