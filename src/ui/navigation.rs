//! Go-to-definition/find-references' actual navigation: a browser-history
//! style jump stack ([`JumpStack`]) and the target-resolution logic
//! ([`navigate_to`]) that decides whether a jump moves the cursor within
//! the diff already on screen or pushes a new [`FileView`] on top of it.
//! Both `gd`/`gr` and `Ctrl-o`/`Ctrl-i` route through the same
//! [`navigate_to`] — back/forward isn't a special "restore exact view"
//! mechanism, it's just another jump to a remembered `(file, line, column)`,
//! which is simpler to reason about and, since it reuses the very same
//! target resolution a fresh jump uses, never drifts out of sync with it.

use crate::diff::ColumnMap;
use crate::lsp::LspManager;
use crate::lsp::client::uri_to_path;
use crate::ui::app::{App, MainPaneFocus};
use crate::ui::file_view::FileView;
use crate::ui::hover_popup::HoverQuery;
use crate::ui::refresh;
use crate::ui::symbols;
use crate::ui::view::{View, ViewStack};
use lsp_types::{GotoDefinitionResponse, Location, PositionEncodingKind};
use std::path::{Path, PathBuf};

/// The most jump-history entries [`JumpStack`] keeps on either side (back or
/// forward) before discarding the oldest — bounds memory for a very long
/// review session without needing the user to ever think about it.
const MAX_HISTORY: usize = 200;

/// One remembered cursor position: enough to reconstruct it via
/// [`navigate_to`], the same way a fresh go-to-definition/references jump
/// would. `col` is a *display* column (see [`crate::diff::ColumnMap`]), not
/// an LSP-encoded one — it's read back by [`App::jump_cursor_to`]/
/// [`FileView::jump_cursor_to`], never sent to a server.
///
/// `line` is `None` for a *structural* destination — a diff file header
/// selected from the file tree, say — that has no source line of its own to
/// land on. Every jump this module resolves ultimately reaches
/// [`crate::ui::refresh::locate_in_diff`] (via [`App::row_for_target`]),
/// which already has to fall back to "the file's first row" for a
/// shifted/removed line; representing "there was never a line to begin
/// with" the same way, rather than inventing a sentinel like `0`, means one
/// lookup path serves both, and a `Ctrl-o`/`Ctrl-i` round trip through a
/// header is exact instead of silently landing on line 0 there and then
/// failing to round-trip back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpEntry {
    pub file: PathBuf,
    pub git_root: PathBuf,
    pub line: Option<u32>,
    pub col: usize,
}

impl From<&HoverQuery> for JumpEntry {
    fn from(query: &HoverQuery) -> Self {
        Self {
            file: query.file.clone(),
            git_root: query.git_root.clone(),
            line: Some(query.line),
            col: query.display_col,
        }
    }
}

/// Back/forward jump history, browser-history style: [`Self::push`] records
/// where a jump is about to leave from and clears whatever forward history
/// a previous [`Self::back`] had rewound past — the same rule every
/// browser's history stack uses, since a fresh jump from a "gone back to"
/// position makes the old forward path no longer where "forward" should
/// lead.
#[derive(Default)]
pub struct JumpStack {
    back: Vec<JumpEntry>,
    forward: Vec<JumpEntry>,
}

impl JumpStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `from` — where the cursor was just before a jump — onto the
    /// back stack, and clears forward history.
    pub fn push(&mut self, from: JumpEntry) {
        self.forward.clear();
        push_capped(&mut self.back, from);
    }

    /// Moves back one entry, if there is one: records `current` (if given —
    /// see its docs on when it isn't) onto the forward stack and returns
    /// the entry to jump to.
    pub fn back(&mut self, current: Option<JumpEntry>) -> Option<JumpEntry> {
        let target = self.back.pop()?;
        if let Some(current) = current {
            push_capped(&mut self.forward, current);
        }
        Some(target)
    }

    /// The inverse of [`Self::back`].
    pub fn forward(&mut self, current: Option<JumpEntry>) -> Option<JumpEntry> {
        let target = self.forward.pop()?;
        if let Some(current) = current {
            push_capped(&mut self.back, current);
        }
        Some(target)
    }

    #[cfg(test)]
    fn back_len(&self) -> usize {
        self.back.len()
    }
}

fn push_capped(stack: &mut Vec<JumpEntry>, entry: JumpEntry) {
    stack.push(entry);
    if stack.len() > MAX_HISTORY {
        stack.remove(0);
    }
}

/// Flattens a `textDocument/definition` response's three possible shapes
/// (LSP lets a server answer with one location, several, or richer
/// `LocationLink`s carrying separate "what to underline"/"where to go"
/// ranges) into one list of locations — [`navigate_to`] and the references
/// panel only ever need "where do these point," not which shape the server
/// chose to answer in.
pub fn definition_locations(response: GotoDefinitionResponse) -> Vec<Location> {
    match response {
        GotoDefinitionResponse::Scalar(location) => vec![location],
        GotoDefinitionResponse::Array(locations) => locations,
        GotoDefinitionResponse::Link(links) => links
            .into_iter()
            .map(|link| Location {
                uri: link.target_uri,
                range: link.target_selection_range,
            })
            .collect(),
    }
}

/// Converts a single LSP `Location` (as returned by `textDocument/definition`
/// when there's exactly one candidate — the case that jumps straight there
/// instead of opening the references panel) into a [`Target`]: reads the
/// target file to convert its range's start position (in `encoding`, the
/// responding server's negotiated position encoding) into a display column.
/// `None` when the location's URI isn't a resolvable local file or that
/// file can't be read — a jump has to land somewhere real.
pub fn location_to_target(
    location: &Location,
    git_root: &Path,
    encoding: &PositionEncodingKind,
) -> Option<Target> {
    let file = uri_to_path(&location.uri)?;
    let content = std::fs::read_to_string(&file).ok()?;
    let line_text = content
        .lines()
        .nth(location.range.start.line as usize)
        .unwrap_or("");
    let columns = ColumnMap::new(line_text);
    let col = if encoding.as_str() == "utf-8" {
        columns.utf8_to_display(location.range.start.character as usize)
    } else {
        columns.utf16_to_display(location.range.start.character as usize)
    };
    Some(JumpEntry {
        file,
        git_root: git_root.to_path_buf(),
        line: Some(location.range.start.line),
        col,
    })
}

/// Where to jump: an absolute file, a 0-based line, and a display column —
/// [`navigate_to`]'s one parameter type, built either directly from a
/// [`JumpEntry`] (back/forward) or from
/// [`crate::ui::refs_panel::build_entries`]'s conversion of an LSP
/// `Location`.
pub type Target = JumpEntry;

/// Moves the cursor to `target`, deciding how based on where `target` is:
///
/// - If it falls on a `(file, new_line)` the diff at the *bottom* of
///   `stack` already shows (regardless of which view is currently on top —
///   see [`ViewStack::root_mut`]), every `FileView` pushed by earlier jumps
///   is popped and the diff's own cursor moves there. A go-to-definition
///   landing back inside the code under review should show that review, not
///   leave a stale `FileView` in the way.
/// - Else if the view already on top is a `FileView` already showing
///   `target.file`, its cursor just moves — no need to push a second view
///   over the same file.
/// - Else a new `FileView` is read from disk and pushed, with its cursor
///   already on `target`, and [`LspManager::warm_up`] is asked to `didOpen`
///   it so diagnostics (and any second jump within it) work immediately
///   rather than waiting for a hover to trigger the open lazily.
///
/// When `record_history` is set, `from` (the position the jump leaves from,
/// when there is one — see [`View::jump_entry`] on when there isn't) is
/// recorded via [`record_jump`] once the jump has actually landed somewhere
/// — never before: a failed read or an unreachable target must leave
/// `jump_stack` untouched, so the push happens after every fallible step,
/// right before each success return, rather than unconditionally up front
/// the way an earlier version of this function did. `Ctrl-o`/`Ctrl-i` pass
/// `false` here: they manage `jump_stack` themselves before calling this,
/// via [`JumpStack::back`]/[`JumpStack::forward`].
pub fn navigate_to(
    stack: &mut ViewStack,
    jump_stack: &mut JumpStack,
    lsp_manager: &LspManager,
    from: Option<JumpEntry>,
    target: Target,
    record_history: bool,
) -> Result<(), String> {
    // `record_history` doubles as the lookup-precision selector, because
    // the two caller classes split exactly along it today: a fresh
    // definition/reference jump (recording) carries a position the server
    // just resolved, so only an exact row may claim it — anything else
    // falls through to opening the real file below. A history traversal
    // (`Ctrl-o`/`Ctrl-i`, not recording) returns to a remembered position
    // whose content may have drifted, where the nearest-line tolerance is
    // the point — see `App::row_for_target`'s docs.
    let root_row = match stack.root_mut() {
        View::Diff(app) => app.row_for_target(&target.file, target.line, !record_history),
        // The root view is never `Timeline`/`Log` in practice — `ktmr
        // timeline`/`ktmr log`'s root is one of those, but neither entry
        // point ever calls `navigate_to` (no LSP, no jumps) — and `File`
        // has no diff rows to target either way.
        View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => None,
    };
    if let Some(row_idx) = root_row {
        stack.pop_to_root();
        if let View::Diff(app) = stack.top_mut() {
            app.jump_cursor_to(row_idx, target.col);
        }
        if record_history {
            record_jump(jump_stack, from, Some(target));
        }
        return Ok(());
    }

    if let View::File(file) = stack.top_mut()
        && file.file_path() == Some(target.file.as_path())
    {
        file.jump_cursor_to(target.line.unwrap_or(0) as usize, target.col);
        if record_history {
            record_jump(jump_stack, from, Some(target));
        }
        return Ok(());
    }

    let content = std::fs::read_to_string(&target.file)
        .map_err(|e| format!("reading {}: {e}", target.file.display()))?;
    let display_path = target
        .file
        .strip_prefix(&target.git_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| target.file.display().to_string());

    let mut view = FileView::with_hover_target(
        display_path,
        &content,
        Some((target.file.clone(), target.git_root.clone())),
    );
    view.jump_cursor_to(target.line.unwrap_or(0) as usize, target.col);
    let highlight_skipped = view.highlight_skipped;
    stack.push(View::File(view));

    // Large/lockfile-ish files skip warm-up too — see
    // `FileView::highlight_skipped`'s docs and `ui::warm_up_root`, which
    // this mirrors for the "jumped to a file, rather than opened one at
    // startup" path.
    if !highlight_skipped {
        lsp_manager.warm_up(std::slice::from_ref(&target.file), &target.git_root);
    }
    if record_history {
        record_jump(jump_stack, from, Some(target));
    }
    Ok(())
}

/// Centralizes "does this jump count as significant" for every source of a
/// jump `ui::mod` records history for (definition/reference navigation via
/// [`navigate_to`] itself, search confirm, diagnostic stepping): records
/// `from` onto `jump_stack`'s back history only once `to` is known —
/// callers only ever have a destination once a jump has actually landed
/// somewhere, so a failed/no-result action naturally never reaches this at
/// all — and only when `from` and `to` genuinely differ, which suppresses a
/// redundant entry for a jump that resolves back to the position already
/// current (including ordinary movement that produced no jump at all: `from`
/// and `to` captured immediately before/after with nothing in between that
/// moved the cursor are simply equal).
pub(crate) fn record_jump(
    jump_stack: &mut JumpStack,
    from: Option<JumpEntry>,
    to: Option<JumpEntry>,
) {
    if let (Some(from), Some(to)) = (from, to)
        && from != to
    {
        jump_stack.push(from);
    }
}

/// [`App::confirm_files_selection`]'s three-way result (issue #15) — a plain
/// `Option<JumpEntry>` (#14's own shape) can't distinguish "there was
/// nothing to confirm" from "confirming toggled a directory, which produces
/// no jump at all," and `ui::mod`'s `Action::Confirm` arm needs to tell
/// those apart to decide whether to record jump history and invalidate an
/// open hover popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesConfirmOutcome {
    /// Nothing was selected to confirm — reachable on an empty diff
    /// (`visible_rows` itself empty), and, since issue #21's
    /// [`App::click_files_row`] shares this same outcome type, also an
    /// out-of-range or blank-space click (row index past `visible_rows`'
    /// end) landing nowhere real.
    NoSelection,
    /// The selection was a directory row: its collapsed state was flipped
    /// (see [`App::toggle_directory`]). This never leaves the files pane's
    /// own focus, let alone jumps the diff cursor, so `ui::mod` must record
    /// no jump history and invalidate no hover popup for this outcome —
    /// unlike `Opened`, nothing under the diff cursor changed.
    Toggled,
    /// The selection was a file row: the diff cursor jumped to its header.
    /// Which pane holds focus afterwards depends on the producer — Enter
    /// ([`App::confirm_files_selection`]) hands focus to `Diff` exactly as
    /// every #14 confirm did, while a mouse click
    /// ([`App::click_files_row`]) keeps `Files` so repeated clicks and
    /// `j`/`k` continue browsing the tree (issue #21 req 2).
    Opened(JumpEntry),
}

impl App {
    /// The cursor's current absolute file/line/column, if it sits on
    /// content that has one — looser than [`Self::hover_query`] (no symbol
    /// under the cursor required, defaulting `col` to `0`), since this
    /// feeds `Ctrl-o`/`Ctrl-i`'s "where am I right now" rather than an LSP
    /// request, and a jump should be able to return to a plain position a
    /// symbol-seeking hover query would refuse.
    fn current_position(&self) -> Option<(PathBuf, u32, usize)> {
        use crate::diff::{RenderRow, lsp_target};
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
        let (relative, line) = lsp_target(row, file)?;
        let col = symbols::scan(&row.text)
            .get(self.active_symbol)
            .map(|s| s.display_start)
            .unwrap_or(0);
        Some((self.repo_root.join(relative), line, col))
    }

    /// The shared body behind [`Self::confirm_files_selection`] (`Enter`,
    /// issue #14/#15) and [`Self::click_files_row`] (a mouse click, issue
    /// #21): resolves `idx` in `visible_rows` and applies the row's own
    /// keyboard-equivalent transition — a directory row toggles its
    /// collapsed state (mirroring `Space`/`Action::ToggleDirectory` — see
    /// [`Self::toggle_directory`]'s docs), producing no jump; a file row
    /// jumps the diff cursor to its header row via [`Self::jump_cursor_to`],
    /// which sets `focus = Diff` unconditionally (see its own docs) — the
    /// sidebar's one and only way to actually move the diff, since every
    /// other action while `Files` is focused only moves `files_selection`
    /// (see [`Self::update`]'s `MainPaneFocus::Files` arms). `keep_files_focus`
    /// re-asserts `Files` afterward for the click path (req 2: repeated
    /// clicks, and `j`/`k` right after one, should keep browsing the tree
    /// without a focus round trip) — the keyboard path leaves `Diff`
    /// focused exactly as it always has, unaffected by this parameter.
    /// Always writes `files_selection = idx` first (even before the
    /// out-of-range check would matter — mirroring req 5/out-of-range's
    /// "no mutation" only through the early return, not by skipping the
    /// write on a hit) so a click can select a row the keyboard cursor
    /// hadn't reached yet. See [`FilesConfirmOutcome`] for what callers do
    /// with each case.
    fn confirm_row(&mut self, idx: usize, keep_files_focus: bool) -> FilesConfirmOutcome {
        let Some(row) = self.visible_rows.get(idx) else {
            return FilesConfirmOutcome::NoSelection;
        };
        self.files_selection = idx;
        match row.kind {
            crate::ui::file_tree::VisibleKind::Directory { .. } => {
                let path = row.id.path.clone();
                self.toggle_directory(&path);
                FilesConfirmOutcome::Toggled
            }
            crate::ui::file_tree::VisibleKind::File { file_idx } => {
                let display_path = self.files[file_idx].display_path().to_owned();
                // `find_first_row_of_file` (the tail of `locate_in_diff`'s
                // fallback chain) always succeeds here: `display_path` was
                // just read from `self.files` itself, and `flatten` gives
                // every file a `FileHeader` row unconditionally — this
                // `let...else` is defensive, not a real path.
                let Some(target_row) =
                    refresh::locate_in_diff(&self.files, &self.rows, &display_path, None)
                else {
                    return FilesConfirmOutcome::NoSelection;
                };
                self.jump_cursor_to(target_row, 0);
                if keep_files_focus {
                    self.focus = MainPaneFocus::Files;
                }
                FilesConfirmOutcome::Opened(JumpEntry {
                    file: self.repo_root.join(&display_path),
                    git_root: self.repo_root.clone(),
                    line: None,
                    col: 0,
                })
            }
        }
    }

    /// `Enter` while the files pane has focus — unchanged since issue #14/
    /// #15 (see [`Self::confirm_row`], the body this now delegates to):
    /// confirms whatever `files_selection` already points at, always
    /// handing focus to `Diff` on a file row.
    pub fn confirm_files_selection(&mut self) -> FilesConfirmOutcome {
        self.confirm_row(self.files_selection, false)
    }

    /// A primary click on visible tree row `idx` (issue #21, req 1/2):
    /// focuses `Files` first — so a click while `Diff` owns focus still
    /// resolves against the sidebar's own transitions, not the diff pane's
    /// — then confirms that row exactly like `Enter` would, except a file
    /// row hands focus straight back to `Files` (see [`Self::confirm_row`]'s
    /// `keep_files_focus`) so repeated clicks, and `j`/`k` right after one,
    /// keep browsing the tree without an intervening Tab. Only ever called
    /// with the sidebar on screen to click at all — the `debug_assert`
    /// documents that invariant rather than enforcing it at runtime (an
    /// event that outraces a `sidebar_visible` toggle can't reach here in
    /// the first place, since [`mouse::handle_left_click`](crate::ui::mouse::handle_left_click)
    /// only calls this after [`mouse::files_row_at`](crate::ui::mouse::files_row_at)
    /// finds a row, which the hidden sidebar never records a hit-testable
    /// rect for).
    pub fn click_files_row(&mut self, idx: usize) -> FilesConfirmOutcome {
        debug_assert!(
            self.sidebar_visible,
            "a files-tree click can't reach here with the sidebar hidden"
        );
        // Bounds first, focus second: an out-of-range index must be a true
        // no-op (req 5's blank-space rule) — the wired mouse path already
        // bounds-checks in `files_row_at`, but this is a `pub fn`, and a
        // future caller shouldn't be able to flip focus with an index that
        // resolves to nothing.
        if idx >= self.visible_rows.len() {
            return FilesConfirmOutcome::NoSelection;
        }
        self.focus = MainPaneFocus::Files;
        self.confirm_row(idx, true)
    }
}

impl FileView {
    fn current_position(&self) -> Option<(PathBuf, u32, usize)> {
        let file = self.file_path()?.to_path_buf();
        let col = symbols::scan(&self.cursor_line_text())
            .get(self.active_symbol)
            .map(|s| s.display_start)
            .unwrap_or(0);
        Some((file, self.cursor as u32, col))
    }
}

impl View {
    /// [`JumpEntry`] for the cursor's current position, for
    /// `Ctrl-o`/`Ctrl-i` to record before navigating away — `None` when the
    /// cursor isn't anywhere a jump could usefully return to (a diff's
    /// file/hunk header, a `Del` row, an empty file).
    pub fn jump_entry(&self) -> Option<JumpEntry> {
        match self {
            View::Diff(app) => app.current_position().map(|(file, line, col)| JumpEntry {
                file,
                git_root: app.repo_root.clone(),
                line: Some(line),
                col,
            }),
            View::File(file_view) => {
                file_view
                    .current_position()
                    .map(|(file, line, col)| JumpEntry {
                        file,
                        git_root: file_view.git_root().to_path_buf(),
                        line: Some(line),
                        col,
                    })
            }
            // Read-only, LSP-free — see `TimelineView::hover_query`'s /
            // `LogView::hover_query`'s docs — so there's nowhere a jump
            // could return to.
            View::Timeline(_) | View::Log(_) | View::LspInspector(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffFile, DiffHunk, DiffLineKind, DiffRow};
    use std::sync::Arc;
    use std::sync::mpsc;

    fn entry(n: u32) -> JumpEntry {
        JumpEntry {
            file: PathBuf::from(format!("/repo/src/f{n}.rs")),
            git_root: PathBuf::from("/repo"),
            line: Some(n),
            col: 0,
        }
    }

    /// A structural destination — no source line at all, the shape a diff
    /// file header (or any other future non-line jump target) takes.
    fn header_entry() -> JumpEntry {
        JumpEntry {
            file: PathBuf::from("/repo/src/f1.rs"),
            git_root: PathBuf::from("/repo"),
            line: None,
            col: 0,
        }
    }

    #[test]
    fn back_returns_none_with_empty_history() {
        let mut stack = JumpStack::new();
        assert_eq!(stack.back(Some(entry(0))), None);
    }

    #[test]
    fn push_then_back_returns_the_pushed_entry() {
        let mut stack = JumpStack::new();
        stack.push(entry(1));
        assert_eq!(stack.back(Some(entry(2))), Some(entry(1)));
    }

    #[test]
    fn back_then_forward_round_trips() {
        let mut stack = JumpStack::new();
        stack.push(entry(1));
        let went_back_to = stack.back(Some(entry(2))).unwrap();
        assert_eq!(went_back_to, entry(1));
        let went_forward_to = stack.forward(Some(went_back_to)).unwrap();
        assert_eq!(went_forward_to, entry(2));
    }

    /// [`JumpStack`] itself never inspects `JumpEntry::line` — it stores
    /// and returns whatever was pushed — so a structural entry (no source
    /// line) round-trips through `back`/`forward` exactly like an ordinary
    /// one. What #12 actually adds is [`JumpEntry::line`] being able to
    /// hold `None` at all, plus [`navigate_to`] being able to resolve such
    /// an entry as a target (see the `navigate_to_*` tests below); the
    /// stack's own push/pop mechanics were already generic over the
    /// entry's contents.
    #[test]
    fn a_structural_entry_with_no_line_round_trips_through_back_and_forward() {
        let mut stack = JumpStack::new();
        stack.push(header_entry());
        let went_back_to = stack.back(Some(entry(2))).unwrap();
        assert_eq!(went_back_to, header_entry());
        let went_forward_to = stack.forward(Some(went_back_to)).unwrap();
        assert_eq!(went_forward_to, entry(2));
    }

    #[test]
    fn a_fresh_push_clears_forward_history() {
        let mut stack = JumpStack::new();
        stack.push(entry(1)); // back=[1]
        // Going back from "current position" entry(2) pops entry(1) and
        // remembers entry(2) as what `forward` should return to.
        assert_eq!(stack.back(Some(entry(2))), Some(entry(1)));
        assert_eq!(stack.forward(None), Some(entry(2)));

        // Rewind again, then push a genuinely new jump — the old forward
        // entry (2, already consumed above) must not resurface, and
        // neither should anything else: a fresh push always clears
        // forward history.
        stack.push(entry(1));
        stack.back(Some(entry(3)));
        stack.push(entry(4));
        assert_eq!(stack.forward(None), None);
    }

    #[test]
    fn history_is_capped_and_drops_the_oldest_entry() {
        let mut stack = JumpStack::new();
        for n in 0..MAX_HISTORY as u32 + 5 {
            stack.push(entry(n));
        }
        assert_eq!(stack.back_len(), MAX_HISTORY);
        // The oldest entries (0..5) were evicted; the most recent one
        // pushed is still there.
        assert_eq!(stack.back(None), Some(entry(MAX_HISTORY as u32 + 4)));
    }

    #[test]
    fn definition_locations_flattens_all_three_response_shapes() {
        let loc = Location {
            uri: "file:///a.rs".parse().unwrap(),
            range: lsp_types::Range::default(),
        };
        assert_eq!(
            definition_locations(GotoDefinitionResponse::Scalar(loc.clone())),
            vec![loc.clone()]
        );
        assert_eq!(
            definition_locations(GotoDefinitionResponse::Array(vec![
                loc.clone(),
                loc.clone()
            ])),
            vec![loc.clone(), loc.clone()]
        );

        let link = lsp_types::LocationLink {
            origin_selection_range: None,
            target_uri: loc.uri.clone(),
            target_range: loc.range,
            target_selection_range: loc.range,
        };
        assert_eq!(
            definition_locations(GotoDefinitionResponse::Link(vec![link])),
            vec![loc]
        );
    }

    // ---- record_jump ---------------------------------------------------

    #[test]
    fn record_jump_pushes_from_when_positions_differ() {
        let mut stack = JumpStack::new();
        record_jump(&mut stack, Some(entry(1)), Some(entry(2)));
        assert_eq!(stack.back(None), Some(entry(1)));
    }

    #[test]
    fn record_jump_suppresses_a_jump_that_resolves_to_the_same_position() {
        let mut stack = JumpStack::new();
        record_jump(&mut stack, Some(entry(1)), Some(entry(1)));
        assert_eq!(stack.back(None), None, "no-op jump must not create history");
    }

    #[test]
    fn record_jump_does_nothing_without_both_a_from_and_a_to() {
        let mut stack = JumpStack::new();
        record_jump(&mut stack, None, Some(entry(1)));
        record_jump(&mut stack, Some(entry(1)), None);
        record_jump(&mut stack, None, None);
        assert_eq!(stack.back(None), None);
    }

    /// `record_jump` is the one funnel every significant-jump source calls
    /// through — `navigate_to` for definition/reference navigation,
    /// `ui::mod::handle_action`'s search-confirm and
    /// `NextDiagnostic`/`PrevDiagnostic` arms directly. From `record_jump`'s
    /// own point of view a "definition" jump and a "diagnostic" jump are
    /// indistinguishable, both just a `(from, to)` pair, so exercising it
    /// directly with synthetic entries standing in for each source is a
    /// faithful proxy for the mixed real-world sequence this pins down:
    /// definition/reference, search, and diagnostic jumps, interleaved with
    /// ordinary movement — which never calls `record_jump` at all (see
    /// `App::update`'s `CursorDown`/`CursorUp`/... arms), so there's
    /// nothing to simulate for it beyond its plain absence from history.
    #[test]
    fn record_jump_calls_from_different_sources_share_one_chronological_history() {
        let mut stack = JumpStack::new();

        // A "definition" jump: line 1 -> line 5.
        record_jump(&mut stack, Some(entry(1)), Some(entry(5)));
        // A "search" jump: line 5 -> line 9.
        record_jump(&mut stack, Some(entry(5)), Some(entry(9)));
        // A "diagnostic" jump: line 9 -> line 2.
        record_jump(&mut stack, Some(entry(9)), Some(entry(2)));

        // `Ctrl-o` three times retraces exactly these three jumps in
        // reverse chronological order, regardless of which feature caused
        // each one.
        assert_eq!(stack.back(Some(entry(2))), Some(entry(9)));
        assert_eq!(stack.back(Some(entry(9))), Some(entry(5)));
        assert_eq!(stack.back(Some(entry(5))), Some(entry(1)));
        assert_eq!(stack.back(Some(entry(1))), None, "history exhausted");
    }

    // ---- navigate_to -----------------------------------------------------

    fn test_lsp_manager() -> LspManager {
        let (tx, _rx) = mpsc::channel();
        LspManager::new(tx, Arc::new(std::collections::HashMap::new()), false)
    }

    /// A one-file, one-hunk `App` wrapped in its own root [`ViewStack`] —
    /// `flatten`'s row order for this shape is `[FileHeader, HunkHeader,
    /// Line, Line, ...]`, so row `0` is always the file header and row
    /// `i + 2` is `lines[i]`.
    fn diff_app_stack(repo_root: &Path, file_name: &str, lines: &[&str]) -> ViewStack {
        let rows: Vec<DiffRow> = lines
            .iter()
            .enumerate()
            .map(|(i, text)| DiffRow {
                kind: DiffLineKind::Context,
                text: (*text).to_owned(),
                old_line: Some(i as u32 + 1),
                new_line: Some(i as u32 + 1),
            })
            .collect();
        let file = DiffFile {
            old_path: Some(file_name.to_owned()),
            new_path: Some(file_name.to_owned()),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: lines.len() as u32,
                new_start: 1,
                new_lines: lines.len() as u32,
                header: String::new(),
                known_eof: true,
                rows,
            }],
            ..Default::default()
        };
        let app = App::new("repo".to_owned(), repo_root.to_path_buf(), vec![file]);
        ViewStack::new(View::Diff(app))
    }

    #[test]
    fn navigate_to_a_structural_target_lands_on_the_files_header_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut stack = diff_app_stack(dir.path(), "a.rs", &["one", "two"]);
        let mut jump_stack = JumpStack::new();
        let lsp_manager = test_lsp_manager();

        let target = JumpEntry {
            file: dir.path().join("a.rs"),
            git_root: dir.path().to_path_buf(),
            line: None,
            col: 0,
        };
        navigate_to(
            &mut stack,
            &mut jump_stack,
            &lsp_manager,
            None,
            target,
            false,
        )
        .unwrap();

        assert!(
            stack.is_at_root(),
            "a target already inside the root diff must never push a view"
        );
        let View::Diff(app) = stack.top() else {
            panic!("expected the root diff");
        };
        assert_eq!(app.cursor, 0, "row 0 is the file header");
    }

    #[test]
    fn navigate_to_a_present_line_moves_the_root_diffs_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let mut stack = diff_app_stack(dir.path(), "a.rs", &["one", "two"]);
        let mut jump_stack = JumpStack::new();
        let lsp_manager = test_lsp_manager();

        let target = JumpEntry {
            file: dir.path().join("a.rs"),
            git_root: dir.path().to_path_buf(),
            line: Some(1), // "two" — new_line 2, 0-based
            col: 0,
        };
        navigate_to(
            &mut stack,
            &mut jump_stack,
            &lsp_manager,
            None,
            target,
            false,
        )
        .unwrap();

        assert!(stack.is_at_root());
        let View::Diff(app) = stack.top() else {
            panic!("expected the root diff");
        };
        assert_eq!(app.cursor, 3, "row 3 is the Line row for \"two\"");
    }

    #[test]
    fn a_fresh_jump_outside_the_diffs_rows_opens_the_real_file_not_the_nearest_row() {
        let dir = tempfile::tempdir().unwrap();
        // The diff renders only lines 1-2 of a.rs; the definition target
        // sits far outside them, at a line the on-disk file really has.
        let mut stack = diff_app_stack(dir.path(), "a.rs", &["one", "two"]);
        let disk: String = (1..=60).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.path().join("a.rs"), disk).unwrap();
        let mut jump_stack = JumpStack::new();
        let lsp_manager = test_lsp_manager();

        let target = JumpEntry {
            file: dir.path().join("a.rs"),
            git_root: dir.path().to_path_buf(),
            line: Some(50),
            col: 0,
        };
        navigate_to(
            &mut stack,
            &mut jump_stack,
            &lsp_manager,
            None,
            target,
            true, // a fresh jump: exact row or the real file, never "nearest"
        )
        .unwrap();

        assert!(
            matches!(stack.top(), View::File(_)),
            "a fresh jump to a line this diff never rendered must open the \
             real file, not silently land on the numerically-nearest row"
        );
    }

    #[test]
    fn a_history_return_to_a_drifted_line_lands_on_the_nearest_remaining_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut stack = diff_app_stack(dir.path(), "a.rs", &["one", "two"]);
        let mut jump_stack = JumpStack::new();
        let lsp_manager = test_lsp_manager();

        let target = JumpEntry {
            file: dir.path().join("a.rs"),
            git_root: dir.path().to_path_buf(),
            line: Some(50), // a remembered position the content drifted from
            col: 0,
        };
        navigate_to(
            &mut stack,
            &mut jump_stack,
            &lsp_manager,
            None,
            target,
            false, // Ctrl-o/Ctrl-i: drift tolerance is the point
        )
        .unwrap();

        assert!(
            stack.is_at_root(),
            "a tolerant history return stays inside the diff already shown"
        );
        let View::Diff(app) = stack.top() else {
            panic!("expected the root diff");
        };
        assert_eq!(
            app.cursor, 3,
            "row 3 (\"two\") is the nearest remaining row"
        );
    }

    #[test]
    fn navigate_to_records_history_only_once_the_jump_has_actually_landed() {
        let dir = tempfile::tempdir().unwrap();
        let mut stack = diff_app_stack(dir.path(), "a.rs", &["one", "two"]);
        let mut jump_stack = JumpStack::new();
        let lsp_manager = test_lsp_manager();

        let from = JumpEntry {
            file: dir.path().join("a.rs"),
            git_root: dir.path().to_path_buf(),
            line: Some(0),
            col: 0,
        };
        let target = JumpEntry {
            file: dir.path().join("a.rs"),
            git_root: dir.path().to_path_buf(),
            line: Some(1),
            col: 0,
        };
        navigate_to(
            &mut stack,
            &mut jump_stack,
            &lsp_manager,
            Some(from.clone()),
            target,
            true,
        )
        .unwrap();
        assert_eq!(jump_stack.back(None), Some(from));
    }

    #[test]
    fn navigate_to_does_not_record_a_jump_that_resolves_to_the_same_position() {
        let dir = tempfile::tempdir().unwrap();
        let mut stack = diff_app_stack(dir.path(), "a.rs", &["one", "two"]);
        let mut jump_stack = JumpStack::new();
        let lsp_manager = test_lsp_manager();

        let here = JumpEntry {
            file: dir.path().join("a.rs"),
            git_root: dir.path().to_path_buf(),
            line: Some(0),
            col: 0,
        };
        navigate_to(
            &mut stack,
            &mut jump_stack,
            &lsp_manager,
            Some(here.clone()),
            here,
            true,
        )
        .unwrap();
        assert_eq!(jump_stack.back(None), None);
    }

    #[test]
    fn navigate_to_an_unreadable_file_fails_and_leaves_history_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let mut stack = diff_app_stack(dir.path(), "a.rs", &["one", "two"]);
        let mut jump_stack = JumpStack::new();
        let lsp_manager = test_lsp_manager();

        let from = JumpEntry {
            file: dir.path().join("a.rs"),
            git_root: dir.path().to_path_buf(),
            line: Some(0),
            col: 0,
        };
        // Not present in the diff (so the root-diff branch declines) and
        // not on disk either (so the "push a fresh `FileView`" branch's
        // read fails) — a target that can't resolve any way `navigate_to`
        // knows how to try.
        let missing = JumpEntry {
            file: dir.path().join("does-not-exist.rs"),
            git_root: dir.path().to_path_buf(),
            line: Some(0),
            col: 0,
        };
        let result = navigate_to(
            &mut stack,
            &mut jump_stack,
            &lsp_manager,
            Some(from),
            missing,
            true,
        );
        assert!(result.is_err());
        assert_eq!(
            jump_stack.back(None),
            None,
            "a failed jump must not corrupt history"
        );
    }

    // ---- confirm_files_selection (issue #14) ------------------------------

    /// Two one-line files — enough for `App::current_position`/`jump_entry`
    /// to have a real source location to leave from in file 0, and a real
    /// header to jump to in file 1. Row order per file is `[FileHeader,
    /// HunkHeader, Line]`, so file 0's content row is index 2 and file 1's
    /// header is index 3.
    fn two_file_app(repo_root: &Path) -> crate::ui::app::App {
        let make = |name: &str| DiffFile {
            old_path: Some(name.to_owned()),
            new_path: Some(name.to_owned()),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                header: String::new(),
                known_eof: true,
                rows: vec![DiffRow {
                    kind: DiffLineKind::Context,
                    text: "content".to_owned(),
                    old_line: Some(1),
                    new_line: Some(1),
                }],
            }],
            ..Default::default()
        };
        crate::ui::app::App::new(
            "repo".to_owned(),
            repo_root.to_path_buf(),
            vec![make("a.rs"), make("b.rs")],
        )
    }

    #[test]
    fn confirm_files_selection_lands_on_the_selected_files_header_and_focuses_diff() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = two_file_app(dir.path());
        app.cursor = 2; // file 0's content row — a real place to jump from
        app.focus = crate::ui::app::MainPaneFocus::Files;
        app.files_selection = 1; // b.rs

        let FilesConfirmOutcome::Opened(to) = app.confirm_files_selection() else {
            panic!("a two-file diff's file row must open, not toggle or no-op");
        };
        assert_eq!(to.file, dir.path().join("b.rs"));
        assert_eq!(to.line, None, "a file header is a structural destination");
        assert_eq!(app.cursor, 3, "row 3 is b.rs's own file header");
        assert_eq!(app.focus, crate::ui::app::MainPaneFocus::Diff);
    }

    /// [`View::jump_entry`]'s exact `View::Diff` computation, without
    /// needing to move `app` into a `View` (which would stop the test from
    /// using it afterward) — `App::current_position` is the same private
    /// method that arm calls, reachable here since `tests` is a descendant
    /// module of the one it's defined in.
    fn jump_entry_for(app: &App, git_root: &Path) -> Option<JumpEntry> {
        app.current_position().map(|(file, line, col)| JumpEntry {
            file,
            git_root: git_root.to_path_buf(),
            line: Some(line),
            col,
        })
    }

    #[test]
    fn confirm_files_selection_round_trips_through_the_jump_stack() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = two_file_app(dir.path());
        app.cursor = 2; // a real source location in file 0
        app.focus = crate::ui::app::MainPaneFocus::Files;
        app.files_selection = 1;

        let from = jump_entry_for(&app, dir.path());
        let FilesConfirmOutcome::Opened(to) = app.confirm_files_selection() else {
            panic!("a two-file diff's file row must open, not toggle or no-op");
        };
        let mut jump_stack = JumpStack::new();
        record_jump(&mut jump_stack, from.clone(), Some(to));
        assert_eq!(
            jump_stack.back(None),
            from,
            "Ctrl-o must be able to retrace a confirm-from-files-selection jump"
        );
    }

    #[test]
    fn confirm_files_selection_from_a_header_records_nothing() {
        // Cursor already on a header row (index 0): `jump_entry` has no
        // source location to leave from, so `record_jump` — even given a
        // real destination — must record nothing (mirrors
        // `record_jump_does_nothing_without_both_a_from_and_a_to`, exercised
        // here through the real `confirm_files_selection` caller instead of
        // a synthetic entry).
        let dir = tempfile::tempdir().unwrap();
        let mut app = two_file_app(dir.path());
        assert_eq!(app.cursor, 0);
        let from = jump_entry_for(&app, dir.path());
        assert_eq!(from, None, "a header row has no source location");

        app.focus = crate::ui::app::MainPaneFocus::Files;
        app.files_selection = 1;
        let FilesConfirmOutcome::Opened(to) = app.confirm_files_selection() else {
            panic!("a two-file diff's file row must open, not toggle or no-op");
        };
        let mut jump_stack = JumpStack::new();
        record_jump(&mut jump_stack, from, Some(to));
        assert_eq!(jump_stack.back(None), None);
    }

    #[test]
    fn confirm_files_selection_on_an_empty_diff_reports_no_selection() {
        let dir = tempfile::tempdir().unwrap();
        let mut app =
            crate::ui::app::App::new("repo".to_owned(), dir.path().to_path_buf(), Vec::new());
        app.focus = crate::ui::app::MainPaneFocus::Files;
        assert_eq!(
            app.confirm_files_selection(),
            FilesConfirmOutcome::NoSelection
        );
    }

    /// Issue #15: confirming a directory row toggles it instead of jumping
    /// the diff cursor — the sidebar's selection and focus both stay on
    /// `Files`, unlike a file row's `Opened`.
    #[test]
    fn confirm_files_selection_on_a_directory_row_toggles_it_without_jumping() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = crate::ui::app::App::new(
            "repo".to_owned(),
            dir.path().to_path_buf(),
            vec![DiffFile {
                new_path: Some("src/lib.rs".to_owned()),
                old_path: Some("src/lib.rs".to_owned()),
                hunks: vec![DiffHunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 1,
                    header: String::new(),
                    known_eof: true,
                    rows: vec![DiffRow {
                        kind: DiffLineKind::Context,
                        text: "content".to_owned(),
                        old_line: Some(1),
                        new_line: Some(1),
                    }],
                }],
                ..Default::default()
            }],
        );
        app.focus = crate::ui::app::MainPaneFocus::Files;
        app.files_selection = 0; // the "src" directory row

        assert_eq!(app.confirm_files_selection(), FilesConfirmOutcome::Toggled);
        assert_eq!(
            app.focus,
            crate::ui::app::MainPaneFocus::Files,
            "toggling a directory must not hand focus to Diff"
        );
        assert_eq!(app.cursor, 0, "the diff cursor must not move either");
    }

    // ---- click_files_row (issue #21) --------------------------------------

    /// The req-2 crux, and the one place a click's outcome actually differs
    /// from `Enter`'s: `jump_cursor_to` (called from inside `confirm_row`)
    /// sets `focus = Diff` unconditionally as its own first line, exactly
    /// as [`confirm_files_selection_lands_on_the_selected_files_header_and_focuses_diff`]
    /// pins down for the keyboard path — `click_files_row` must flip it
    /// straight back to `Files` afterward rather than leaving that Diff
    /// focus in place, so repeated clicks (and `j`/`k` right after one)
    /// keep browsing the tree without an intervening Tab.
    #[test]
    fn click_files_row_on_a_file_opens_it_but_leaves_focus_on_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = two_file_app(dir.path());
        app.cursor = 2; // file 0's content row
        app.focus = crate::ui::app::MainPaneFocus::Diff;
        app.files_selection = 0;

        let FilesConfirmOutcome::Opened(to) = app.click_files_row(1) else {
            panic!("a two-file diff's file row must open, not toggle or no-op");
        };
        assert_eq!(to.file, dir.path().join("b.rs"));
        assert_eq!(app.cursor, 3, "row 3 is b.rs's own file header");
        assert_eq!(app.files_selection, 1, "the click selects the clicked row");
        assert_eq!(
            app.focus,
            crate::ui::app::MainPaneFocus::Files,
            "a click must keep Files focused, unlike Enter's confirm"
        );
    }

    #[test]
    fn click_files_row_round_trips_through_the_jump_stack() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = two_file_app(dir.path());
        app.cursor = 2; // a real source location in file 0
        app.focus = crate::ui::app::MainPaneFocus::Diff;

        let from = jump_entry_for(&app, dir.path());
        let FilesConfirmOutcome::Opened(to) = app.click_files_row(1) else {
            panic!("a two-file diff's file row must open, not toggle or no-op");
        };
        let mut jump_stack = JumpStack::new();
        record_jump(&mut jump_stack, from.clone(), Some(to));
        assert_eq!(
            jump_stack.back(None),
            from,
            "Ctrl-o must be able to retrace a click's jump exactly like Enter's"
        );
    }

    /// req 5: a click past the end of `visible_rows` (blank space below the
    /// last row, or a stale index from a frame that's already moved on)
    /// reports `NoSelection` and leaves everything — selection, focus,
    /// cursor — untouched, the same "early return before any mutation"
    /// shape `confirm_row` already gives an empty diff.
    #[test]
    fn click_files_row_out_of_range_reports_no_selection_and_mutates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = two_file_app(dir.path());
        app.focus = crate::ui::app::MainPaneFocus::Diff;
        app.files_selection = 0;
        let cursor_before = app.cursor;

        assert_eq!(
            app.click_files_row(99),
            FilesConfirmOutcome::NoSelection,
            "only two files exist — index 99 is well past the tree"
        );
        assert_eq!(
            app.files_selection, 0,
            "an out-of-range click must not move the selection"
        );
        assert_eq!(app.cursor, cursor_before);
        assert_eq!(
            app.focus,
            crate::ui::app::MainPaneFocus::Diff,
            "an out-of-range click is a true no-op — focus included"
        );
    }
}
