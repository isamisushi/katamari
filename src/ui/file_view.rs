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
use crate::ui::mouse::{FrameGeometry, HitRow, LineHit, ScrollTarget};
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
    /// active symbol when recording this position on the jump stack. A thin
    /// wrapper over [`Self::line_text_at`] at the cursor's own line — see
    /// that method's docs for why the two are split.
    pub fn cursor_line_text(&self) -> String {
        self.line_text_at(self.cursor)
    }

    /// As [`Self::cursor_line_text`], for an arbitrary line rather than only
    /// the cursor's own — issue #24's passive pointer hover needs to scan
    /// symbols on whatever line the pointer is resting on, without moving
    /// `cursor` there first (mirrors [`crate::ui::app::App::row_text`]).
    fn line_text_at(&self, idx: usize) -> String {
        self.highlighter
            .line(idx)
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
    /// `file_path`) or the current line has no identifier-like token. A thin
    /// wrapper over [`Self::hover_query_for`] at the cursor's own line and
    /// active symbol — see that method's docs for why the two are split.
    pub fn hover_query(&self) -> Option<HoverQuery> {
        self.hover_query_for(self.cursor, self.active_symbol)
    }

    /// As [`crate::ui::app::App::hover_query_for`], for an arbitrary line/
    /// symbol-index pair — extracted so issue #24's passive pointer hover
    /// (via [`Self::hover_query_at`]) can build the identical query a
    /// keyboard hover would for whatever line the pointer rests on, without
    /// moving `cursor`/`active_symbol` (req 3).
    fn hover_query_for(&self, row_idx: usize, symbol_idx: usize) -> Option<HoverQuery> {
        let file = self.file_path.clone()?;
        let line_text = self.line_text_at(row_idx);
        let symbol = symbols::scan(&line_text).get(symbol_idx).copied()?;
        Some(HoverQuery {
            file,
            git_root: self.git_root.clone(),
            line: row_idx as u32,
            line_text,
            display_col: symbol.display_start,
        })
    }

    /// As [`crate::ui::app::App::hover_query_at`] — resolves the symbol from
    /// a raw display column, requiring a real match (no [`Self::resolve_active_symbol`]-style
    /// symbol-`0` fallback: see that method's docs for why a pointer resting
    /// on whitespace must resolve to no target at all).
    pub fn hover_query_at(&self, row_idx: usize, display_col: usize) -> Option<HoverQuery> {
        let (symbol_idx, matched) = self.symbol_at(row_idx, display_col);
        if !matched {
            return None;
        }
        self.hover_query_for(row_idx, symbol_idx)
    }

    /// The number of lines in the opened file — the file view's analogue of
    /// `App::rows.len()`.
    pub fn line_count(&self) -> usize {
        self.highlighter.line_count()
    }

    /// The content pane's visible row count changes on terminal resize; the
    /// event loop reports it before each frame, mirroring `App`. As
    /// [`crate::ui::app::App::set_viewport_height`], a no-op when `height`
    /// is unchanged — issue #20's `Self::scroll_by` moves `scroll_offset`
    /// without moving `self.cursor`, and this is called every event-loop
    /// iteration regardless of an actual resize, so re-clamping
    /// unconditionally would snap a wheel-scrolled `FileView` right back
    /// before the next draw ever showed it.
    pub fn set_viewport_height(&mut self, height: usize) {
        let height = height.max(1);
        if height == self.viewport_height {
            return;
        }
        self.viewport_height = height;
        self.clamp_scroll();
    }

    /// As [`crate::ui::app::App::set_content_width`] — refreshed every
    /// frame alongside [`Self::set_viewport_height`] so [`Self::row_visual_height`]'s
    /// wrap width stays current with the pane's actual size. Skips the
    /// clamp when `width` is unchanged, for the same reason
    /// [`Self::set_viewport_height`] does.
    pub fn set_content_width(&mut self, width: usize) {
        let width = width.max(1);
        if width == self.content_width {
            return;
        }
        self.content_width = width;
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
            // A real no-op, not an action this view never sees: an opened
            // file is a single pane, so there's nothing for Tab/BackTab to
            // cycle focus between — same reasoning as `App::update`'s
            // identical arm (see that method's comment).
            Action::FocusNextPane | Action::FocusPrevPane => {}
            // `ui::mod` intercepts all of these before they reach here,
            // same as in `App::update` — see that method's comment. `Quit`
            // joins this bucket rather than `TimelineView`/`LogView`'s own
            // should_quit-setting arms: it's intercepted even earlier, at
            // the keymap resolver, before a matched action is dispatched
            // anywhere (see `ui::mod::event_loop`'s
            // `StepResult::Matched(Action::Quit)` arm) — global quit, not a
            // per-view "close."
            Action::Hover
            | Action::Cancel
            | Action::GotoDefinition
            | Action::FindReferences
            | Action::NextDiagnostic
            | Action::PrevDiagnostic
            | Action::JumpBack
            | Action::JumpForward
            | Action::Confirm
            | Action::Quit => {}
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
            | Action::ToggleDirectory
            | Action::OpenScopeMenu
            // Same reason as `OpenScopeMenu` just above: `FileView` has no
            // "branch vs base" concept of its own to swap onto either.
            | Action::ReviewBranchVsBase
            // Same bucket as `ToggleLspInspector`: reachable from any view,
            // needs the agent handle `App`/`FileView` don't own — see
            // `crate::acp::session`.
            | Action::ToggleAgentPanel
            | Action::PushCommentsToAgent
            | Action::CancelAgentTurn
            | Action::AddComment
            | Action::ToggleComments
            | Action::ExpandFold
            | Action::CollapseFold
            | Action::OpenSearch
            | Action::NextMatch
            | Action::PrevMatch
            // Issue #16: visual-line selection is a diff-view-only concept
            // too, same reasoning as `ExpandFold`/`OpenSearch` above — a
            // single opened file has no logical `RenderRow`s of its own to
            // select over. Issue #17's `YankSelection` joins it for the
            // same reason: nothing to yank without a selection to yank.
            | Action::ToggleVisualLine
            | Action::YankSelection
            // Same story: asking the agent about a selection is a
            // diff-view-only concept, no different from `AddComment`.
            | Action::AskAgent => {}
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

    /// Issue #20's wheel vocabulary — as [`crate::ui::app::App::scroll_by`],
    /// moving `scroll_offset` directly by `delta` visual rows without
    /// touching `self.cursor`. No `clamp_scroll` call afterward for the
    /// same reason `App::scroll_by` skips it: that method derives
    /// `scroll_offset` from the cursor, which would just undo this. The
    /// next cursor-moving key press runs it anyway and self-heals.
    pub fn scroll_by(&mut self, delta: isize) {
        if self.line_count() == 0 {
            return;
        }
        let last_row = self.line_count() - 1;
        self.scroll_offset = scroll::scroll_by(
            self.scroll_offset,
            delta,
            last_row,
            self.viewport_height,
            |i| self.row_visual_height(i),
        );
    }

    /// As [`crate::ui::app::App::resolve_active_symbol`] — see that
    /// method's docs.
    fn resolve_active_symbol(&self, display_col: usize) -> (usize, bool) {
        self.symbol_at(self.cursor, display_col)
    }

    /// As [`crate::ui::app::App::symbol_at`], for an arbitrary line —
    /// extracted for the same reason [`Self::line_text_at`] was.
    fn symbol_at(&self, row_idx: usize, display_col: usize) -> (usize, bool) {
        let symbols = symbols::scan(&self.line_text_at(row_idx));
        match symbols
            .iter()
            .position(|s| s.display_start <= display_col && display_col < s.display_end)
        {
            Some(idx) => (idx, true),
            None => (0, false),
        }
    }

    /// Moves the cursor to `line` and, if the active symbol there covers
    /// `display_col`, selects it — mirrors [`crate::ui::app::App::jump_cursor_to`];
    /// see that method's docs for why centering rather than clamping.
    pub fn jump_cursor_to(&mut self, line: usize, display_col: usize) {
        self.cursor = line.min(self.line_count().saturating_sub(1));
        self.active_symbol = self.resolve_active_symbol(display_col).0;
        self.scroll_offset = scroll::center(self.cursor, self.viewport_height, |i| {
            self.row_visual_height(i)
        });
        self.clamp_scroll();
    }

    /// As [`crate::ui::app::App::position_cursor_from_click`] — a mouse
    /// click's cursor landing, without `jump_cursor_to`'s `scroll::center`
    /// (see that method's docs on why), returning whether the click landed
    /// on a symbol.
    pub fn position_cursor_from_click(&mut self, line: usize, display_col: usize) -> bool {
        self.cursor = line.min(self.line_count().saturating_sub(1));
        let (active_symbol, matched) = self.resolve_active_symbol(display_col);
        self.active_symbol = active_symbol;
        self.clamp_scroll();
        matched
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

/// The real content [`Rect`] [`render`] draws into, carved out of `area` by
/// the same `Borders::LEFT` block `render` itself renders — issue #22 needs
/// this a second time (`ui::mod`'s `draw`, to record alongside the `HitRow`s
/// [`render`] hands back) without recomputing the border math by hand, the
/// same "one border-geometry function, not a second hand-counted literal"
/// rule `pane::inner_rect` follows for the diff pane (see that function's
/// docs).
pub fn content_rect(area: Rect) -> Rect {
    Block::default().borders(Borders::LEFT).inner(area)
}

/// Renders the file pane and returns one [`crate::ui::mouse::HitRow`] per
/// rendered terminal row within it, in the same viewport-clamped order the
/// `Line`s themselves were pushed — `ui::mod::draw` pairs this with
/// [`content_rect`] and hands both to [`FrameGeometry::record_file_content`]
/// so issue #22's click resolution shares this exact render loop rather than
/// re-deriving line wrapping a second time (see this module's `mouse` import
/// and `crate::ui::mouse`'s own doc comment).
pub fn render(
    frame: &mut Frame,
    area: Rect,
    view: &FileView,
    diagnostics: &DiagnosticsStore,
    geometry: &mut FrameGeometry,
) -> Vec<HitRow> {
    geometry.record(area, ScrollTarget::FilePane);
    let block = Block::default().borders(Borders::LEFT).title(" file ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width as usize;
    let viewport_height = inner.height as usize;
    let wrap = crate::config::wrap_enabled();

    let mut lines: Vec<Line> = Vec::with_capacity(viewport_height);
    let mut hits: Vec<HitRow> = Vec::with_capacity(viewport_height);
    let mut idx = view.scroll_offset;
    while lines.len() < viewport_height && idx < view.line_count() {
        for (line, hit) in content_line(view, idx, idx == view.cursor, width, diagnostics, wrap) {
            if lines.len() >= viewport_height {
                break;
            }
            lines.push(line);
            hits.push(HitRow::FileLine(hit));
        }
        idx += 1;
    }

    frame.render_widget(Paragraph::new(lines), inner);
    hits
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
pub(crate) fn gutter_width() -> usize {
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

/// Issue #22: returns each visual row's [`Line`] paired with the
/// [`LineHit`] locating it back in `idx`'s own logical line (`row_idx ==
/// idx`, `content_start_col` accumulating across wrap the same way
/// `col_offset` below always has) — [`render`] wraps each pair as a
/// [`HitRow::FileLine`] and threads it alongside the `Line` itself into its
/// own parallel `Vec`, never a second computation.
fn content_line(
    view: &FileView,
    idx: usize,
    is_cursor: bool,
    width: usize,
    diagnostics: &DiagnosticsStore,
    wrap: bool,
) -> Vec<(Line<'static>, LineHit)> {
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
        out.push((
            line,
            LineHit {
                row_idx: idx,
                content_start_col: col_offset,
            },
        ));
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

    /// End-to-end through the *real* pipeline — `render`'s emitted
    /// `HitRow`s + `content_rect` recorded into a `FrameGeometry`, a click
    /// resolved via `file_row_hit`/`resolve_hit`, then applied with
    /// `position_cursor_from_click` — rather than hand-built `HitRow`
    /// fixtures, which can quietly encode the same wrong assumption twice
    /// (e.g. `content_start_col` captured after the width increment
    /// instead of before, or `content_rect` drifting from `render`'s own
    /// `Block::inner`). A wrapped continuation row is the case where every
    /// one of those mistakes shows.
    #[test]
    fn a_click_resolved_through_the_real_render_pipeline_lands_on_wrapped_rows() {
        use crate::ui::mouse::{self, FrameGeometry};
        use ratatui::backend::TestBackend;

        // Line 0 wraps (its tail lands on a continuation row); line 1 is a
        // plain second line rendered after the continuation.
        let source = format!("{}tail_symbol\nlet second = 2;\n", "a".repeat(40));
        let mut view = FileView::with_hover_target("src/main.rs".to_owned(), &source, None);
        let area = Rect {
            x: 5,
            y: 2,
            width: 40,
            height: 8,
        };
        view.set_viewport_height(content_rect(area).height as usize);
        view.set_content_width(content_width_for_pane(area.width));

        let backend = TestBackend::new(60, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let diagnostics = DiagnosticsStore::new();
        let mut geometry = FrameGeometry::new();
        let mut hits = Vec::new();
        terminal
            .draw(|frame| {
                hits = render(frame, area, &view, &diagnostics, &mut geometry);
            })
            .unwrap();
        geometry.record_file_content(content_rect(area), hits);

        // The second terminal row inside the pane is line 0's continuation
        // row (line 0's text exceeds the ~30-column content budget once).
        // Click on it, past the gutter, at the column where "tail_symbol"
        // renders — the resolver must map that back to line 0 and a
        // display column *inside* the tail, not to a fresh row-1 position.
        let inner = content_rect(area);
        let content_width = content_width_for_pane(area.width);
        let tail_start_in_row = 40 % content_width; // columns of 'a' on the continuation row
        let click_col = inner.x + (gutter_width() + tail_start_in_row + 2) as u16;
        let click_row = inner.y + 1;
        let (local_x, _y, hit) = geometry
            .file_row_hit(click_col, click_row)
            .expect("the continuation row is a recorded hit row");
        let resolved = mouse::resolve_hit(*hit, gutter_width(), local_x)
            .expect("a continuation content cell resolves");
        assert_eq!(resolved.row_idx, 0, "continuation rows belong to line 0");
        assert_eq!(
            resolved.display_col,
            40 + 2,
            "display column continues the logical line across the wrap"
        );

        assert!(
            view.position_cursor_from_click(resolved.row_idx, resolved.display_col),
            "the click lands inside tail_symbol — a real symbol match"
        );
        assert_eq!(view.cursor, 0);
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

    // ---- scroll_by (issue #20 wheel routing) ---------------------------

    #[test]
    fn scroll_by_moves_the_offset_without_touching_the_cursor() {
        let mut view = test_view();
        view.update(Action::Bottom);
        let cursor_before = view.cursor;
        view.scroll_by(1);
        assert_eq!(
            view.cursor, cursor_before,
            "wheel scroll never moves the cursor"
        );
        assert!(view.scroll_offset > 0);
    }

    #[test]
    fn scroll_by_clamps_at_the_top() {
        let mut view = test_view();
        view.scroll_by(-5);
        assert_eq!(view.scroll_offset, 0);
    }

    #[test]
    fn scroll_by_clamps_at_the_last_useful_offset() {
        let mut view = test_view();
        view.scroll_by(1000);
        let maxed = view.scroll_offset;
        view.scroll_by(1000);
        assert_eq!(view.scroll_offset, maxed);
    }

    #[test]
    fn scroll_by_offset_self_heals_on_the_next_cursor_move() {
        let mut view = test_view();
        view.update(Action::Bottom);
        let settled_offset = view.scroll_offset;
        view.scroll_by(-2);
        assert_ne!(view.scroll_offset, settled_offset);
        view.update(Action::CursorDown); // already at the bottom, cursor unchanged
        assert_eq!(view.scroll_offset, settled_offset);
    }

    #[test]
    fn scroll_by_is_a_no_op_on_an_empty_file() {
        let mut view = FileView::with_hover_target("empty.rs".to_owned(), "", None);
        view.set_viewport_height(2);
        view.scroll_by(5);
        assert_eq!(view.scroll_offset, 0);
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

    // ---- resolve_active_symbol / position_cursor_from_click (issue #22) --

    /// A source tall enough for `position_cursor_from_click_never_recenters_unlike_jump_cursor_to`
    /// to exercise a real divergence between `scroll::center` and plain
    /// `clamp_scroll` — see `App`'s identical test for why a too-small
    /// viewport/line-count makes the two coincide by accident.
    fn tall_source() -> String {
        (0..30).map(|i| format!("line_{i} value\n")).collect()
    }

    #[test]
    fn resolve_active_symbol_matches_the_symbol_a_column_falls_within() {
        let mut view = test_view();
        view.update(Action::CursorDown); // "    let x = 1;"
        // "let" spans [4, 7); pick a column inside it, not just its start.
        assert_eq!(view.resolve_active_symbol(5), (0, true));
    }

    #[test]
    fn resolve_active_symbol_falls_back_to_symbol_zero_on_whitespace() {
        let view = test_view();
        // Column 2 of "fn main() {" is still inside the leading "fn" — use
        // column 11, the trailing brace, which no symbol covers.
        assert_eq!(view.resolve_active_symbol(11), (0, false));
    }

    // ---- hover_query_at (issue #24) ---------------------------------------

    /// As [`test_view`], but with a real hover target — [`Self::hover_query`]/
    /// [`Self::hover_query_at`] both refuse unconditionally when `file_path`
    /// is `None`, which `test_view` deliberately is (see that fixture's
    /// docs), so the hover-query tests below need their own fixture.
    fn test_view_with_target() -> FileView {
        let source = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        let mut view = FileView::with_hover_target(
            "src/main.rs".to_owned(),
            source,
            Some((PathBuf::from("/repo/src/main.rs"), PathBuf::from("/repo"))),
        );
        view.set_viewport_height(2);
        view
    }

    #[test]
    fn hover_query_at_matches_hover_query_built_from_the_same_row_and_column() {
        let mut view = test_view_with_target();
        view.update(Action::CursorDown); // "    let x = 1;"
        let expected = view.hover_query().expect("row 1 has an eligible symbol");
        // Moved off the row afterward — `hover_query_at` must reproduce the
        // identical query without the cursor sitting on the target row at
        // all (req 3: passive hover never moves `cursor`/`active_symbol`).
        view.update(Action::CursorDown);
        view.update(Action::CursorDown);
        assert_ne!(view.cursor, 1);
        assert_eq!(view.hover_query_at(1, 5), Some(expected));
    }

    #[test]
    fn hover_query_at_is_none_without_a_hover_target() {
        let view = test_view(); // built with `target: None`
        assert_eq!(view.hover_query_at(0, 3), None);
    }

    #[test]
    fn hover_query_at_on_whitespace_is_none_unlike_the_click_paths_symbol_zero_fallback() {
        // Column 11 of "fn main() {" is the trailing brace — no symbol
        // covers it (see `resolve_active_symbol_falls_back_to_symbol_zero_on_whitespace`
        // above for the click path's opposite, symbol-0 fallback).
        let view = test_view_with_target();
        assert_eq!(view.hover_query_at(0, 11), None);
    }

    #[test]
    fn position_cursor_from_click_moves_the_cursor_and_reports_whether_it_matched() {
        // "fn main() {" — "fn" spans [0, 2), "main" spans [3, 7).
        let mut view = test_view();
        let matched = view.position_cursor_from_click(0, 3); // "main"
        assert!(matched);
        assert_eq!(view.cursor, 0);
        assert_eq!(view.active_symbol, 1);

        let matched = view.position_cursor_from_click(0, 1); // still inside "fn"
        assert!(matched);
        assert_eq!(view.active_symbol, 0);

        let matched = view.position_cursor_from_click(0, 2); // the space before "main"
        assert!(!matched);
        assert_eq!(
            view.active_symbol, 0,
            "falls back to symbol 0, same as jump_cursor_to"
        );
    }

    #[test]
    fn position_cursor_from_click_never_recenters_unlike_jump_cursor_to() {
        let source = tall_source();
        let mut clicked = FileView::with_hover_target("f.rs".to_owned(), &source, None);
        clicked.set_viewport_height(6);
        let mut centered = FileView::with_hover_target("f.rs".to_owned(), &source, None);
        centered.set_viewport_height(6);

        let target = clicked.line_count() / 2;
        centered.jump_cursor_to(target, 0);
        clicked.position_cursor_from_click(target, 0);

        assert_eq!(
            clicked.cursor, centered.cursor,
            "both land on the same line"
        );
        assert_ne!(
            clicked.scroll_offset, centered.scroll_offset,
            "position_cursor_from_click must not recenter the way jump_cursor_to does"
        );
    }
}
