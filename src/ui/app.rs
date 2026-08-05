//! Application state and its pure state transitions. Deliberately free of
//! any terminal or ratatui dependency: `App` can be constructed and driven
//! entirely with parsed diff data and [`Action`]s, which is what makes it
//! testable without a real terminal.

use crate::diff::{DiffFile, RenderRow, SideBySideRow, flatten, flatten_side_by_side, lsp_target};
use crate::keymap::Action;
use crate::lsp::DiagnosticsStore;
use crate::ui::hover_popup::HoverQuery;
use crate::ui::refresh;
use crate::ui::scroll;
use crate::ui::symbols;
use std::path::{Path, PathBuf};

/// Which of the two diff layouts the diff pane renders. Toggled by
/// [`Action::ToggleLayout`]; [`crate::ui::diff_view`] may still render
/// unified even when this is [`Layout::SideBySide`] if the pane is too
/// narrow to show two columns (see `diff_view::effective_layout`) — that
/// fallback is a rendering concern, so it doesn't change this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    #[default]
    Unified,
    SideBySide,
}

impl Layout {
    fn toggled(self) -> Self {
        match self {
            Layout::Unified => Layout::SideBySide,
            Layout::SideBySide => Layout::Unified,
        }
    }
}

/// All state the UI needs to render a frame and respond to input.
pub struct App {
    pub repo_name: String,
    /// Absolute repository root. Every path in `files` is relative to this
    /// — it's what turns a diff's `new_path` into a file `Action::Hover`
    /// can actually open, and it's the boundary
    /// [`crate::lsp::adapter::workspace_root`] searches within when it
    /// walks up from that file looking for a `Cargo.toml`.
    pub repo_root: PathBuf,
    pub files: Vec<DiffFile>,
    pub rows: Vec<RenderRow>,
    /// Del/add-run pairing of `rows` for the side-by-side layout, computed
    /// once at load time alongside `rows` rather than per frame — see
    /// [`crate::diff::flatten_side_by_side`].
    pub side_by_side_rows: Vec<SideBySideRow>,
    pub cursor: usize,
    pub scroll_offset: usize,
    /// Index into the current row's [`symbols::scan`] output — which
    /// identifier-like token `Action::Hover` would target. Reset to `0`
    /// whenever the cursor moves to a different row; cycled by
    /// `Action::NextSymbol`/`PrevSymbol` without moving the cursor.
    pub active_symbol: usize,
    pub sidebar_visible: bool,
    pub layout: Layout,
    pub pending_keys: String,
    pub should_quit: bool,
    /// Whether this session is running `ktmr diff --watch` — purely a
    /// display flag for the status bar's "⦿ watch" indicator; `ui::mod`'s
    /// event loop decides independently (via whether it was handed a
    /// [`crate::ui::PreRefreshHook`]) whether to actually spawn a watcher,
    /// so this field can never drift into claiming watch mode is on when
    /// nothing is watching.
    pub watch_mode: bool,
    viewport_height: usize,
}

impl App {
    pub fn new(repo_name: String, repo_root: PathBuf, files: Vec<DiffFile>) -> Self {
        let rows = flatten(&files);
        let side_by_side_rows = flatten_side_by_side(&files);
        Self {
            repo_name,
            repo_root,
            files,
            rows,
            side_by_side_rows,
            cursor: 0,
            scroll_offset: 0,
            active_symbol: 0,
            sidebar_visible: true,
            layout: Layout::default(),
            pending_keys: String::new(),
            should_quit: false,
            watch_mode: false,
            viewport_height: 1,
        }
    }

    /// The raw text of the row at `cursor`, for [`symbols::scan`] — `None`
    /// for a file/hunk header or binary notice, which have no line content
    /// to scan.
    fn cursor_row_text(&self) -> Option<&str> {
        match self.rows.get(self.cursor)? {
            RenderRow::Line {
                file_idx,
                hunk_idx,
                row_idx,
            } => Some(
                self.files[*file_idx].hunks[*hunk_idx].rows[*row_idx]
                    .text
                    .as_str(),
            ),
            _ => None,
        }
    }

    fn cycle_symbol(&mut self, delta: isize) {
        let Some(text) = self.cursor_row_text() else {
            return;
        };
        let symbols = symbols::scan(text);
        if symbols.is_empty() {
            return;
        }
        let len = symbols.len() as isize;
        let next = (self.active_symbol as isize + delta).rem_euclid(len);
        self.active_symbol = next as usize;
    }

    /// What `Action::Hover` should ask a language server about right now:
    /// the file and 0-based line the cursor's row targets (via
    /// [`lsp_target`]), the line's text (needed to convert the active
    /// symbol's display column into the server's encoding), and that
    /// column itself. `None` when the cursor isn't on an eligible row (a
    /// header, a `Del` line, a deleted/binary file) or the row has no
    /// identifier-like token to target.
    pub fn hover_query(&self) -> Option<HoverQuery> {
        let RenderRow::Line {
            file_idx,
            hunk_idx,
            row_idx,
        } = self.rows.get(self.cursor)?
        else {
            return None;
        };
        let file = &self.files[*file_idx];
        let row = &file.hunks[*hunk_idx].rows[*row_idx];
        let (relative_path, line) = lsp_target(row, file)?;
        let symbol = symbols::scan(&row.text).get(self.active_symbol).copied()?;
        Some(HoverQuery {
            file: self.repo_root.join(relative_path),
            git_root: self.repo_root.clone(),
            line,
            line_text: row.text.clone(),
            display_col: symbol.display_start,
        })
    }

    /// The diff pane's visible row count changes on terminal resize; the
    /// event loop reports it before each frame so half-page scrolling and
    /// scroll-to-cursor clamping use an up-to-date value.
    pub fn set_viewport_height(&mut self, height: usize) {
        self.viewport_height = height.max(1);
        self.clamp_scroll();
    }

    /// Swaps in a freshly re-parsed diff — watch mode's refresh pipeline
    /// calls this after every debounced batch of filesystem changes (see
    /// `ui::mod`'s `handle_watch_refresh`). Preserves the cursor's logical
    /// position via [`refresh::capture_anchor`]/[`refresh::restore_anchor`]
    /// rather than reusing the old flat index blindly, which after an edit
    /// anywhere else in the diff would typically now point at unrelated
    /// content; the scroll offset is restored the same way, keeping the
    /// cursor's on-screen row stable rather than re-centering on every
    /// refresh.
    ///
    /// Returns whether the cursor's landed-on row is *exactly* the row it
    /// was anchored to before the refresh (same file, line, and text) —
    /// `ui::mod` uses this to decide whether an open hover/references
    /// overlay, always anchored to whatever's under the cursor, should
    /// survive the swap or close.
    pub fn apply_refresh(&mut self, files: Vec<DiffFile>) -> bool {
        let anchor =
            refresh::capture_anchor(&self.files, &self.rows, self.cursor, self.scroll_offset);

        self.files = files;
        self.rows = flatten(&self.files);
        self.side_by_side_rows = flatten_side_by_side(&self.files);
        self.active_symbol = 0;

        if self.rows.is_empty() {
            self.cursor = 0;
            self.scroll_offset = 0;
            return false;
        }

        let restored = refresh::restore_anchor(&self.files, &self.rows, &anchor);
        self.cursor = restored.row_index;
        self.scroll_offset = restored
            .row_index
            .saturating_sub(refresh::scroll_delta(&anchor));
        self.clamp_scroll();
        restored.overlay_survives
    }

    /// Index into `files`/`rows` for whichever file the cursor currently
    /// sits within, for sidebar highlighting.
    pub fn selected_file(&self) -> usize {
        match self.rows.get(self.cursor) {
            Some(RenderRow::FileHeader { file_idx }) => *file_idx,
            Some(RenderRow::BinaryNotice { file_idx }) => *file_idx,
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
        let cursor_before = self.cursor;
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
            Action::ToggleLayout => self.layout = self.layout.toggled(),
            Action::NextSymbol => self.cycle_symbol(1),
            Action::PrevSymbol => self.cycle_symbol(-1),
            // `ui::mod`'s event loop intercepts all of these before they
            // reach here — each needs either the LSP manager, the
            // diagnostics store, or the jump stack, none of which `App`
            // owns — so they're no-ops from `App`'s own point of view.
            Action::Hover
            | Action::Cancel
            | Action::GotoDefinition
            | Action::FindReferences
            | Action::NextDiagnostic
            | Action::PrevDiagnostic
            | Action::JumpBack
            | Action::JumpForward
            | Action::Confirm => {}
            // `ui::mod` intercepts `ToggleTimeline` before it reaches here
            // (constructing/tearing down a `TimelineView` isn't a pure state
            // transition); `ToggleRangeSelect` only means something inside
            // `TimelineView` itself.
            Action::ToggleTimeline | Action::ToggleRangeSelect => {}
            Action::Quit => self.should_quit = true,
        }
        if self.cursor != cursor_before {
            self.active_symbol = 0;
        }
        self.clamp_scroll();
    }

    fn half_page(&self) -> usize {
        scroll::half_page(self.viewport_height)
    }

    /// The row index whose `(file, new_line)` matches `target_file`/
    /// `target_line` (0-based), if this diff has one — how a
    /// go-to-definition/references jump decides to move the cursor within
    /// the diff already being reviewed instead of pushing a new `FileView`
    /// on top of it. `target_file` is compared as an absolute path, the
    /// same coordinate space [`Self::hover_query`] reports.
    pub fn row_for_target(&self, target_file: &Path, target_line: u32) -> Option<usize> {
        self.rows.iter().position(|row| {
            let RenderRow::Line {
                file_idx,
                hunk_idx,
                row_idx,
            } = row
            else {
                return false;
            };
            let file = &self.files[*file_idx];
            let line_row = &file.hunks[*hunk_idx].rows[*row_idx];
            match lsp_target(line_row, file) {
                Some((relative, line)) => {
                    line == target_line && self.repo_root.join(&relative) == target_file
                }
                None => false,
            }
        })
    }

    /// Moves the cursor to `row_idx` and, if the active symbol at that row
    /// covers `display_col`, selects it — the destination of a
    /// go-to-definition/references jump, or of `]d`/`[d`. Centers the
    /// scroll offset (rather than the minimal nudge ordinary cursor
    /// movement uses) so a jump's destination lands with surrounding
    /// context visible, not pinned to the viewport's edge.
    pub fn jump_cursor_to(&mut self, row_idx: usize, display_col: usize) {
        self.cursor = row_idx.min(self.rows.len().saturating_sub(1));
        self.active_symbol = self
            .cursor_row_text()
            .map(symbols::scan)
            .and_then(|syms| {
                syms.iter()
                    .position(|s| s.display_start <= display_col && display_col < s.display_end)
            })
            .unwrap_or(0);
        self.scroll_offset = scroll::center(self.cursor, self.viewport_height);
        self.clamp_scroll();
    }

    /// Moves the cursor to the next (`forward`) or previous row whose
    /// `(file, new_line)` carries a diagnostic, wrapping around the end of
    /// the row list — `Action::NextDiagnostic`/`PrevDiagnostic`. A no-op
    /// when no row in the diff has one.
    pub fn jump_to_diagnostic(&mut self, diagnostics: &DiagnosticsStore, forward: bool) {
        let Some(target) = self.find_diagnostic_row(diagnostics, forward) else {
            return;
        };
        self.cursor = target;
        self.active_symbol = 0;
        self.scroll_offset = scroll::center(self.cursor, self.viewport_height);
        self.clamp_scroll();
    }

    fn find_diagnostic_row(&self, diagnostics: &DiagnosticsStore, forward: bool) -> Option<usize> {
        if self.rows.is_empty() {
            return None;
        }
        let has_diagnostic = |idx: usize| -> bool {
            let RenderRow::Line {
                file_idx,
                hunk_idx,
                row_idx,
            } = self.rows[idx]
            else {
                return false;
            };
            let file = &self.files[file_idx];
            let row = &file.hunks[hunk_idx].rows[row_idx];
            let Some((relative, line)) = lsp_target(row, file) else {
                return false;
            };
            diagnostics
                .severity_at(&self.repo_root.join(relative), line)
                .is_some()
        };

        let len = self.rows.len();
        (1..len)
            .map(|step| {
                if forward {
                    (self.cursor + step) % len
                } else {
                    (self.cursor + len - step) % len
                }
            })
            .find(|idx| has_diagnostic(*idx))
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
        self.scroll_offset =
            scroll::clamp_scroll(self.cursor, self.viewport_height, self.scroll_offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_unified_diff;

    const FIXTURE: &str = include_str!("../diff/fixtures/multi_file.diff");

    fn test_app() -> App {
        let files = parse_unified_diff(FIXTURE);
        let mut app = App::new("test-repo".to_owned(), PathBuf::from("/repo"), files);
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
    fn toggle_layout_flips_between_unified_and_side_by_side() {
        let mut app = test_app();
        assert_eq!(app.layout, Layout::Unified);
        app.update(Action::ToggleLayout);
        assert_eq!(app.layout, Layout::SideBySide);
        app.update(Action::ToggleLayout);
        assert_eq!(app.layout, Layout::Unified);
    }

    #[test]
    fn quit_sets_should_quit() {
        let mut app = test_app();
        assert!(!app.should_quit);
        app.update(Action::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn next_symbol_cycles_and_wraps_and_resets_when_cursor_moves() {
        let mut app = test_app();
        app.update(Action::Top);
        app.update(Action::CursorDown); // row 1: the hunk header
        app.update(Action::CursorDown); // row 2: "fn helper() {}" — two symbols

        assert_eq!(app.active_symbol, 0);
        app.update(Action::NextSymbol);
        assert_eq!(app.active_symbol, 1);
        app.update(Action::NextSymbol);
        assert_eq!(
            app.active_symbol, 0,
            "cycling past the last symbol wraps around"
        );
        app.update(Action::PrevSymbol);
        assert_eq!(
            app.active_symbol, 1,
            "cycling before the first symbol wraps backward"
        );

        app.update(Action::CursorDown);
        assert_eq!(
            app.active_symbol, 0,
            "moving to a new row resets the active symbol"
        );
    }

    #[test]
    fn hover_query_targets_the_active_symbol_on_an_add_row() {
        let mut app = test_app();
        app.update(Action::Top);
        for _ in 0..4 {
            app.update(Action::CursorDown);
        }
        // Row 4 is the "+fn new_name() {}" add line in src/lib.rs's hunk.
        let query = app.hover_query().expect("add row is hover-eligible");
        assert_eq!(query.file, PathBuf::from("/repo/src/lib.rs"));
        assert_eq!(query.line, 1); // new_line 2, 0-based
        assert_eq!(query.line_text, "fn new_name() {}");
        assert_eq!(query.display_col, 0); // "fn", the first symbol

        app.update(Action::NextSymbol);
        let query = app.hover_query().expect("still on the same row");
        assert_eq!(query.display_col, 3); // "new_name"
    }

    #[test]
    fn hover_query_is_none_on_a_del_row() {
        let mut app = test_app();
        app.update(Action::Top);
        for _ in 0..3 {
            app.update(Action::CursorDown);
        }
        // Row 3 is the "-fn old_name() {}" del line — nothing to hover.
        assert_eq!(app.hover_query(), None);
    }

    #[test]
    fn scroll_follows_cursor_past_viewport_bottom() {
        let mut app = test_app();
        app.set_viewport_height(3);
        app.update(Action::Bottom);
        assert!(app.cursor >= app.scroll_offset);
        assert!(app.cursor < app.scroll_offset + 3);
    }

    #[test]
    fn apply_refresh_with_an_identical_diff_keeps_the_cursor_in_place_and_reports_survival() {
        let mut app = test_app();
        app.update(Action::Top);
        for _ in 0..4 {
            app.update(Action::CursorDown);
        }
        let cursor_before = app.cursor;

        let same_files = parse_unified_diff(FIXTURE);
        let survived = app.apply_refresh(same_files);

        assert!(
            survived,
            "identical content at the same position must survive"
        );
        assert_eq!(app.cursor, cursor_before);
    }

    #[test]
    fn apply_refresh_on_an_empty_diff_resets_cursor_and_scroll_without_panicking() {
        let mut app = test_app();
        app.update(Action::Bottom);
        let survived = app.apply_refresh(Vec::new());
        assert!(!survived);
        assert_eq!(app.cursor, 0);
        assert_eq!(app.scroll_offset, 0);
        assert!(app.rows.is_empty());
    }
}
