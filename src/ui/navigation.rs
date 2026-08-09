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
use crate::ui::app::App;
use crate::ui::file_view::FileView;
use crate::ui::hover_popup::HoverQuery;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpEntry {
    pub file: PathBuf,
    pub git_root: PathBuf,
    pub line: u32,
    pub col: usize,
}

impl From<&HoverQuery> for JumpEntry {
    fn from(query: &HoverQuery) -> Self {
        Self {
            file: query.file.clone(),
            git_root: query.git_root.clone(),
            line: query.line,
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
        line: location.range.start.line,
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
/// pushed onto `jump_stack` first. `Ctrl-o`/`Ctrl-i` pass `false` here:
/// they manage `jump_stack` themselves before calling this, via
/// [`JumpStack::back`]/[`JumpStack::forward`].
pub fn navigate_to(
    stack: &mut ViewStack,
    jump_stack: &mut JumpStack,
    lsp_manager: &LspManager,
    from: Option<JumpEntry>,
    target: Target,
    record_history: bool,
) -> Result<(), String> {
    if record_history && let Some(from) = from {
        jump_stack.push(from);
    }

    let root_row = match stack.root_mut() {
        View::Diff(app) => app.row_for_target(&target.file, target.line),
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
        return Ok(());
    }

    if let View::File(file) = stack.top_mut()
        && file.file_path() == Some(target.file.as_path())
    {
        file.jump_cursor_to(target.line as usize, target.col);
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
    view.jump_cursor_to(target.line as usize, target.col);
    let highlight_skipped = view.highlight_skipped;
    stack.push(View::File(view));

    // Large/lockfile-ish files skip warm-up too — see
    // `FileView::highlight_skipped`'s docs and `ui::warm_up_root`, which
    // this mirrors for the "jumped to a file, rather than opened one at
    // startup" path.
    if !highlight_skipped {
        lsp_manager.warm_up(std::slice::from_ref(&target.file), &target.git_root);
    }
    Ok(())
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
                line,
                col,
            }),
            View::File(file_view) => {
                file_view
                    .current_position()
                    .map(|(file, line, col)| JumpEntry {
                        file,
                        git_root: file_view.git_root().to_path_buf(),
                        line,
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

    fn entry(n: u32) -> JumpEntry {
        JumpEntry {
            file: PathBuf::from(format!("/repo/src/f{n}.rs")),
            git_root: PathBuf::from("/repo"),
            line: n,
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
}
