//! The jj snapshot timeline: a browsable list of working-copy snapshots
//! (see [`crate::vcs::jj::JjRepo::snapshot_ops`]) alongside the diff between
//! whichever one is selected and the one before it — "review the evolution
//! of the agent's work, not just the cumulative diff," per the milestone's
//! product goal.
//!
//! The right pane is a real [`App`], not a bespoke renderer: rebuilding one
//! from each selection's parsed diff and delegating to
//! [`crate::ui::diff_view::render`] reuses cursor/scroll/hunk-navigation and
//! the whole rendering pipeline for free, at the cost of a small "app inside
//! a view" nesting that [`TimelineView::update`] resolves by forwarding to
//! `diff_app.update` whenever the diff pane has focus. Deliberately read-only
//! and LSP-free (see [`TimelineView::hover_query`]): a past snapshot's
//! content doesn't match the working tree on disk, so hover/goto-definition
//! against it would be misleading rather than merely unavailable.

use crate::diff::{DiffFile, parse_unified_diff};
use crate::keymap::{Action, Keymap};
use crate::lsp::DiagnosticsStore;
use crate::ui::app::{App, Layout};
use crate::ui::hints;
use crate::ui::hover_popup::HoverQuery;
use crate::ui::mouse::{FrameGeometry, ScrollTarget};
use crate::ui::pane::cycle_focus;
use crate::vcs::jj::{JjRepo, SnapshotOp};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout as RatLayout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::path::Path;

/// Which pane `j`/`k`/`gg`/`G`/etc. currently act on. Cycled by
/// `Action::FocusNextPane`/`FocusPrevPane` (Tab/BackTab) through
/// [`cycle_focus`] against [`FOCUS_ORDER`] — issue #13's split of pane
/// focus out of `NextSymbol`/`PrevSymbol`. That split has a real behavior
/// consequence here, not just a rename: before it, Tab/BackTab (then
/// `NextSymbol`/`PrevSymbol`) were intercepted unconditionally at the top
/// of [`TimelineView::update`], which meant a reviewer could never reach
/// the *nested* diff pane's own symbol cycling — every Tab press toggled
/// this `Focus` back to `List` first, no matter which pane already had it.
/// Now that pane focus and symbol selection are different actions, `l`/`h`
/// (or `M-f`/`M-b`) genuinely fall through to `self.diff_app.update` once
/// `Focus::Diff` is active, the same as every other action `update_list`
/// doesn't special-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Diff,
}

const FOCUS_ORDER: [Focus; 2] = [Focus::List, Focus::Diff];

/// Default `limit` for [`JjRepo::snapshot_ops`] — bounds the *raw* op log
/// fetched, not the number of snapshot entries returned; see that method's
/// docs. Shared by `Action::ToggleTimeline`'s handling in
/// [`crate::ui::mod`] and `ktmr timeline`'s CLI entry point in `main.rs`, so
/// the two never drift apart.
pub const DEFAULT_OP_LOG_LIMIT: usize = 100;

const LIST_WIDTH: u16 = 40;

/// State for one open timeline. Construction fetches the op log once;
/// afterward, only the selected diff is (re)fetched — never the whole list
/// — on a selection change or a live snapshot notification (see
/// [`Self::refresh_live`]).
pub struct TimelineView {
    jj_repo: JjRepo,
    limit: usize,
    /// Newest first — [`crate::vcs::jj::JjRepo::snapshot_ops`]'s own order.
    ops: Vec<SnapshotOp>,
    /// Index into `ops` of the primary cursor position.
    selected: usize,
    focus: Focus,
    /// Set by `v`: the other endpoint of a combined range diff. `None` is
    /// the default "diff against the immediately preceding snapshot" mode.
    range_anchor: Option<usize>,
    /// The right pane, rebuilt by [`Self::reload_diff`] whenever the
    /// effective (from, to) pair changes.
    diff_app: App,
    /// Set instead of a real diff when there's nothing to show (no snapshot
    /// history yet, the selection is the oldest loaded snapshot with
    /// nothing earlier to diff against, or the `jj op diff` call itself
    /// failed) — shown in the status bar rather than silently leaving the
    /// diff pane blank.
    diff_note: Option<String>,
    pub pending_keys: String,
    pub should_quit: bool,
    viewport_height: usize,
}

impl TimelineView {
    /// Fetches the op log (bounded by `limit` — see
    /// [`crate::vcs::jj::JjRepo::snapshot_ops`]'s docs on why the returned
    /// list can be shorter than `limit`) and loads the newest snapshot's
    /// diff. The only fallible step is that initial fetch; every subsequent
    /// operation degrades to a `diff_note` instead of an error, since by
    /// then the timeline is already on screen and has no caller left to
    /// propagate a `Result` to.
    pub fn new(jj_repo: JjRepo, limit: usize) -> anyhow::Result<Self> {
        let ops = jj_repo.snapshot_ops(limit)?;
        let mut view = Self {
            jj_repo,
            limit,
            ops,
            selected: 0,
            focus: Focus::List,
            range_anchor: None,
            diff_app: empty_diff_app(Path::new("")),
            diff_note: None,
            pending_keys: String::new(),
            should_quit: false,
            viewport_height: 1,
        };
        view.reload_diff();
        Ok(view)
    }

    pub fn set_viewport_height(&mut self, height: usize) {
        self.viewport_height = height.max(1);
        self.diff_app.set_viewport_height(height);
    }

    /// Always `None`: a past snapshot's content doesn't match the working
    /// tree on disk, so there's nowhere honest to send a hover/goto request
    /// about it — see this module's docs.
    pub fn hover_query(&self) -> Option<HoverQuery> {
        None
    }

    /// A cheap, comparable snapshot of "what's selected," mirroring
    /// [`App::hover_query`]'s cursor-key sibling on the other views —
    /// nothing here ever opens a hover popup (see [`Self::hover_query`]),
    /// but the `View` enum's dispatch still needs one value per variant.
    pub fn cursor_key(&self) -> (usize, usize) {
        (self.selected, self.diff_app.cursor)
    }

    pub fn update(&mut self, action: Action) {
        match action {
            Action::FocusNextPane | Action::FocusPrevPane => {
                self.focus = cycle_focus(
                    &FOCUS_ORDER,
                    self.focus,
                    action == Action::FocusNextPane,
                    |_| true,
                );
                return;
            }
            Action::ToggleRangeSelect => {
                self.range_anchor = match self.range_anchor {
                    Some(_) => None,
                    None => Some(self.selected),
                };
                self.reload_diff();
                return;
            }
            // Enter jumps back to the diff being reviewed; `Esc` closes the
            // same way (see `crate::ui::mod::handle_action`'s `Cancel` arm,
            // which pops exactly one pushed view — this mirrors that
            // outcome locally rather than relying on it, so `update` stays
            // correct even if some future caller reaches it directly). `q`
            // is not part of this: it's intercepted at the keymap resolver
            // as a global quit before a matched action ever reaches here
            // (see `ui::mod::event_loop`'s
            // `StepResult::Matched(Action::Quit)` arm), so it would never
            // arrive as `Action::Quit` in the first place.
            Action::Confirm | Action::Cancel => {
                self.should_quit = true;
                return;
            }
            _ => {}
        }

        match self.focus {
            Focus::List => self.update_list(action),
            Focus::Diff => self.diff_app.update(action),
        }
    }

    fn update_list(&mut self, action: Action) {
        match action {
            Action::CursorDown => self.select(self.selected.saturating_add(1)),
            Action::CursorUp => self.select(self.selected.saturating_sub(1)),
            Action::HalfPageDown => self.select(self.selected.saturating_add(5)),
            Action::HalfPageUp => self.select(self.selected.saturating_sub(5)),
            Action::Top => self.select(0),
            Action::Bottom => self.select(self.ops.len().saturating_sub(1)),
            // Hunk/file navigation, layout toggling, the sidebar, and every
            // LSP-driven action `ui::mod` already intercepts before this
            // point — none of them mean anything against a flat snapshot
            // list.
            _ => {}
        }
    }

    fn select(&mut self, idx: usize) {
        if self.ops.is_empty() {
            return;
        }
        let clamped = idx.min(self.ops.len() - 1);
        if clamped != self.selected {
            self.selected = clamped;
            self.reload_diff();
        }
    }

    /// Issue #20's wheel vocabulary for the snapshot list. Unlike the diff
    /// pane's `scroll_diff_by`, the list has no independent scroll offset
    /// of its own to begin with — `render_list` always windows around
    /// `selected` (see that function's docs), so "scroll the list" and
    /// "move the selection" are the same operation here. That's a real
    /// inconsistency with the diff panes' decoupled cursor/viewport (wheel
    /// there never moves the cursor) — accepted for this issue rather than
    /// inventing a second, list-only scroll-offset concept just for the
    /// wheel; `refs_panel`/`units_panel` share the same shape already.
    /// Goes through `Self::select` (not a direct field write) so a changed
    /// selection reloads the diff exactly once, the same as `j`/`k`.
    pub fn scroll_list_by(&mut self, delta: isize) {
        let target = if delta.is_negative() {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta as usize)
        };
        self.select(target);
    }

    /// As `App::scroll_by`, for the nested diff pane — moves its own
    /// scroll offset without touching its cursor or this view's `selected`.
    pub fn scroll_diff_by(&mut self, delta: isize) {
        self.diff_app.scroll_by(delta);
    }

    /// Rebuilds `diff_app` (and `diff_note`) for the current
    /// selection/range — the one place this view calls out to `jj`
    /// mid-session (every other query happened once, in [`Self::new`] or
    /// [`Self::refresh_live`]).
    fn reload_diff(&mut self) {
        self.diff_note = None;

        if self.ops.is_empty() {
            self.diff_app = empty_diff_app(self.jj_repo.repo_root());
            self.diff_note = Some("no snapshots yet".to_owned());
            self.diff_app.set_viewport_height(self.viewport_height);
            return;
        }

        match resolve_diff_pair(&self.ops, self.selected, self.range_anchor) {
            None => {
                self.diff_app = empty_diff_app(self.jj_repo.repo_root());
                self.diff_note =
                    Some("oldest loaded snapshot — no earlier one to diff against".to_owned());
            }
            Some((from_idx, to_idx)) => {
                let from = &self.ops[from_idx];
                let to = &self.ops[to_idx];
                match self.jj_repo.op_diff(&from.op_id, &to.op_id) {
                    Ok(text) => {
                        let files = parse_unified_diff(&text);
                        self.diff_app =
                            App::new(String::new(), self.jj_repo.repo_root().to_owned(), files);
                    }
                    Err(e) => {
                        self.diff_app = empty_diff_app(self.jj_repo.repo_root());
                        self.diff_note = Some(format!("diff failed: {e}"));
                    }
                }
            }
        }
        self.diff_app.set_viewport_height(self.viewport_height);
    }

    /// Re-fetches the op log after [`crate::ui::JjPreRefreshHook`] reports a
    /// new snapshot, prepending it live rather than waiting for the
    /// timeline to be closed and reopened — keeping whatever was selected
    /// (by op id, not index: a prepended entry shifts every existing
    /// index) selected, per the milestone's requirement that this not
    /// visibly jerk the cursor around mid-review.
    pub fn refresh_live(&mut self) {
        let selected_id = self.ops.get(self.selected).map(|o| o.op_id.clone());
        let anchor_id = self
            .range_anchor
            .and_then(|i| self.ops.get(i))
            .map(|o| o.op_id.clone());

        match self.jj_repo.snapshot_ops(self.limit) {
            Ok(ops) => {
                self.ops = ops;
                self.selected = remap_selection(selected_id.as_deref(), &self.ops);
                self.range_anchor =
                    anchor_id.and_then(|id| self.ops.iter().position(|o| o.op_id == id));
                self.reload_diff();
            }
            Err(e) => self.diff_note = Some(format!("timeline refresh failed: {e}")),
        }
    }
}

fn empty_diff_app(repo_root: &Path) -> App {
    App::new(String::new(), repo_root.to_owned(), Vec::new())
}

/// The pure part of [`TimelineView::reload_diff`]: which two `ops` indices
/// to diff, given the current selection and (if range mode is active) its
/// anchor. `ops` is newest-first, so a *larger* index is an *older*
/// snapshot; the return is always `(older_idx, newer_idx)`, matching `jj op
/// diff --from <older> --to <newer>`'s argument order.
///
/// `None` covers both "nothing to diff yet" cases: no range anchor and the
/// selection is already the oldest loaded snapshot (nothing after it in the
/// list), or a range anchor equal to the current selection (a zero-width
/// range).
fn resolve_diff_pair(
    ops: &[SnapshotOp],
    selected: usize,
    range_anchor: Option<usize>,
) -> Option<(usize, usize)> {
    match range_anchor {
        Some(anchor) if anchor != selected => {
            let (lo, hi) = if anchor < selected {
                (anchor, selected)
            } else {
                (selected, anchor)
            };
            Some((hi, lo))
        }
        Some(_) => None,
        None => {
            let next = selected + 1;
            (next < ops.len()).then_some((next, selected))
        }
    }
}

/// The pure part of [`TimelineView::refresh_live`]: where `old_op_id` (the
/// previously selected op, if any) landed in a freshly fetched `new_ops` —
/// split out so "prepending new snapshots keeps the same one selected" is
/// unit-testable against plain data, without a real jj process. Falls back
/// to `0` (the newest entry) when the old selection isn't found at all,
/// which shouldn't happen in practice (snapshots are only ever prepended,
/// never removed, within one `limit`-bounded fetch) but is a safe default
/// rather than an out-of-bounds index if it ever does.
fn remap_selection(old_op_id: Option<&str>, new_ops: &[SnapshotOp]) -> usize {
    old_op_id
        .and_then(|id| new_ops.iter().position(|o| o.op_id == id))
        .unwrap_or(0)
}

/// `(file_count, added, deleted)` across every file in a parsed diff — the
/// stat annotation shown next to the selected row in the list. Computed
/// from whatever `diff_app` already loaded for the current selection,
/// rather than a separate query, which is what keeps this "lazy": nothing
/// is computed for the 99 rows that aren't selected.
fn diff_stat(files: &[DiffFile]) -> (usize, u32, u32) {
    let (mut added, mut deleted) = (0u32, 0u32);
    for file in files {
        let (a, d) = file.stat();
        added += a;
        deleted += d;
    }
    (files.len(), added, deleted)
}

/// Unix seconds since `time_unix` (never negative — a clock skew between
/// this machine and whatever wrote the operation log is treated as "just
/// now" rather than shown as a negative duration), rendered the way a
/// commit timeline usually is: coarsening from seconds up through days as
/// the gap grows, rather than an exact timestamp nobody needs at a glance.
///
/// `pub(crate)`, not private: [`crate::ui::log_view::LogView`] renders the
/// same "Ns/m/h/d ago" shape for `git log`/`jj log` timestamps and has no
/// reason to duplicate this rather than share it.
pub(crate) fn relative_time(time_unix: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(time_unix);
    let delta = (now - time_unix).max(0);
    match delta {
        0..=59 => format!("{delta}s ago"),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86399 => format!("{}h ago", delta / 3600),
        _ => format!("{}d ago", delta / 86400),
    }
}

/// The first 12 hex characters of a full operation id — enough to
/// disambiguate at a glance in the list without the visual noise of a
/// 128-character hash; never used as a lookup key (every call into
/// [`crate::vcs::jj::JjRepo`] uses the full id `SnapshotOp::op_id` holds).
fn short_op_id(op_id: &str) -> &str {
    &op_id[..op_id.len().min(12)]
}

pub struct Areas {
    pub list: Rect,
    pub diff: Rect,
    pub status: Rect,
}

/// `status_height` is [`hints::required_height`] applied to
/// [`hints::timeline_view_items`] and `area`'s width — see
/// `file_view::layout`'s docs for why the caller computes this rather than
/// a fixed constant.
pub fn layout(area: Rect, status_height: u16) -> Areas {
    let rows = RatLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(status_height)])
        .split(area);
    let cols = RatLayout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(LIST_WIDTH), Constraint::Min(0)])
        .split(rows[0]);
    Areas {
        list: cols[0],
        diff: cols[1],
        status: rows[1],
    }
}

#[allow(clippy::too_many_arguments)] // one render pass threading through
// the same view/render pieces `ui::mod::draw` already juggles for every
// other view; see that function's own justification.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    view: &TimelineView,
    highlighter: &mut crate::highlight::LineHighlighter,
    keymap: &Keymap,
    key_display: &crate::ui::key_display::KeyDisplayState,
    hints_expanded: bool,
    geometry: &mut FrameGeometry,
) {
    let hint_items = hints::timeline_view_items(keymap, hints_expanded);
    let status_height = hints::required_height(&hint_items, area.width);
    let areas = layout(area, status_height);
    geometry.record(areas.list, ScrollTarget::TimelineList);
    geometry.record(areas.diff, ScrollTarget::TimelineDiff);
    render_list(frame, areas.list, view);
    let diagnostics = DiagnosticsStore::new();
    // A past snapshot's diff has nothing in `.katamari/comments.jsonl` to
    // relate to — comments anchor to the working tree, not to jj history —
    // so this always renders with an empty index rather than threading a
    // real one through from `ui::mod`.
    let comments = crate::comments::CommentIndex::default();
    crate::ui::diff_view::render(
        frame,
        areas.diff,
        &view.diff_app,
        highlighter,
        Layout::Unified,
        &diagnostics,
        &comments,
    );
    render_status(frame, areas.status, view, &hint_items);
    crate::ui::key_display::render(frame, areas.diff, key_display);
}

fn render_list(frame: &mut Frame, area: Rect, view: &TimelineView) {
    let block = Block::default().borders(Borders::LEFT).title(" snapshots ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if view.ops.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "(no snapshots yet)",
                Style::default().fg(Color::DarkGray),
            ))),
            inner,
        );
        return;
    }

    let range = view
        .range_anchor
        .map(|anchor| (anchor.min(view.selected), anchor.max(view.selected)));

    // Windows around `selected` the same way `refs_panel::render`/
    // `units_panel::render` do — a pre-existing bug fixed alongside issue
    // #20: without this, a selection past the pane's visible height simply
    // scrolled off the top of an unwindowed `Paragraph` (which clips from
    // its first line, not around the selection), rather than staying on
    // screen the way every other list in this codebase already does.
    let visible_height = (inner.height as usize).max(1);
    let start = view
        .selected
        .saturating_sub(visible_height.saturating_sub(1));

    let lines: Vec<Line> = view
        .ops
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_height)
        .map(|(idx, op)| {
            let is_selected = idx == view.selected;
            let in_range = range.is_some_and(|(lo, hi)| idx >= lo && idx <= hi);

            let mut text = format!(
                "{:>8}  {}",
                relative_time(op.time_unix),
                short_op_id(&op.op_id)
            );
            if is_selected {
                let (files, added, deleted) = diff_stat(&view.diff_app.files);
                text.push_str(&format!("  +{added} -{deleted} ({files} files)"));
            }

            let mut style = Style::default();
            if in_range {
                style = style.bg(Color::Rgb(30, 30, 60));
            }
            if is_selected {
                style = style.add_modifier(Modifier::BOLD);
                if view_focus_is_list(view) {
                    style = style.add_modifier(Modifier::REVERSED);
                }
            }
            Line::from(Span::styled(text, style))
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn view_focus_is_list(view: &TimelineView) -> bool {
    view.focus == Focus::List
}

fn render_status(
    frame: &mut Frame,
    area: Rect,
    view: &TimelineView,
    hint_items: &[hints::HintItem],
) {
    let mut spans = vec![Span::styled(
        " timeline ",
        Style::default().add_modifier(Modifier::BOLD),
    )];

    if view.range_anchor.is_some() {
        spans.push(Span::styled(
            "· range mode ",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if !view.pending_keys.is_empty() {
        spans.push(Span::styled(
            format!("· {} ", view.pending_keys),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if let Some(note) = &view.diff_note {
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

    fn op(id: &str, time: i64) -> SnapshotOp {
        SnapshotOp {
            op_id: id.to_owned(),
            time_unix: time,
            description: "snapshot working copy".to_owned(),
        }
    }

    // ---- resolve_diff_pair -------------------------------------------

    #[test]
    fn no_range_diffs_against_the_immediately_preceding_snapshot() {
        let ops = vec![op("c", 3), op("b", 2), op("a", 1)];
        assert_eq!(resolve_diff_pair(&ops, 0, None), Some((1, 0)));
        assert_eq!(resolve_diff_pair(&ops, 1, None), Some((2, 1)));
    }

    #[test]
    fn no_range_at_the_oldest_snapshot_has_nothing_to_diff_against() {
        let ops = vec![op("c", 3), op("b", 2), op("a", 1)];
        assert_eq!(resolve_diff_pair(&ops, 2, None), None);
    }

    #[test]
    fn range_mode_diffs_the_older_against_the_newer_regardless_of_anchor_order() {
        let ops = vec![op("d", 4), op("c", 3), op("b", 2), op("a", 1)];
        // Anchor newer than selection.
        assert_eq!(resolve_diff_pair(&ops, 3, Some(0)), Some((3, 0)));
        // Anchor older than selection — same pair, order-independent.
        assert_eq!(resolve_diff_pair(&ops, 0, Some(3)), Some((3, 0)));
    }

    #[test]
    fn range_mode_with_a_zero_width_anchor_has_nothing_to_diff() {
        let ops = vec![op("b", 2), op("a", 1)];
        assert_eq!(resolve_diff_pair(&ops, 0, Some(0)), None);
    }

    // ---- remap_selection ------------------------------------------------

    #[test]
    fn selection_stays_on_the_same_op_after_new_snapshots_prepend() {
        let old_selection = op("b", 2);
        let new_ops = vec![op("d", 4), op("c", 3), op("b", 2), op("a", 1)];
        let idx = remap_selection(Some(&old_selection.op_id), &new_ops);
        assert_eq!(idx, 2, "b moved from index 0 to index 2 after two prepends");
    }

    #[test]
    fn missing_old_selection_falls_back_to_the_newest_entry() {
        let new_ops = vec![op("d", 4), op("c", 3)];
        assert_eq!(remap_selection(Some("gone"), &new_ops), 0);
        assert_eq!(remap_selection(None, &new_ops), 0);
    }

    // ---- relative_time ----------------------------------------------------

    #[test]
    fn relative_time_coarsens_from_seconds_to_days() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(relative_time(now - 5), "5s ago");
        assert_eq!(relative_time(now - 125), "2m ago");
        assert_eq!(relative_time(now - 7300), "2h ago");
        assert_eq!(relative_time(now - 200_000), "2d ago");
    }

    #[test]
    fn relative_time_never_goes_negative_on_clock_skew() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(relative_time(now + 1000), "0s ago");
    }

    // ---- short_op_id --------------------------------------------------

    #[test]
    fn short_op_id_truncates_a_full_hash_to_twelve_chars() {
        let full = "a0ecaee5840f10dd5c3c42cf51a2dbf38664b678639fb85539c40af2037a281";
        assert_eq!(short_op_id(full), "a0ecaee5840f");
    }

    #[test]
    fn short_op_id_leaves_a_shorter_string_untouched() {
        assert_eq!(short_op_id("abc"), "abc");
    }

    // ---- TimelineView focus (issue #13) --------------------------------

    /// A minimal real `jj` fixture: a colocated repo with two working-copy
    /// snapshots, the second adding a line with two identifier-like tokens
    /// (`SYMONE SYMTWO`) — enough for `TimelineView::new` to load a
    /// non-empty `diff_app` with a `cycle_symbol`-able row at the bottom.
    /// `crate::vcs::jj`'s own test module has an equivalent fixture, but
    /// it's private to that module and shaped around `jj commit` (real
    /// history) rather than [`JjRepo::snapshot`] (what actually populates
    /// [`JjRepo::snapshot_ops`], the list this view renders) — building a
    /// second, smaller one here is simpler than exporting a shared helper
    /// two call sites would use differently. Every caller skips (rather
    /// than fails) when `jj` isn't on `PATH`, the same convention
    /// `crate::vcs::jj`'s own tests and `tests/e2e/lsp_readiness.rs`'s
    /// `python3_available` guard both follow.
    fn jj_timeline_fixture() -> Option<(tempfile::TempDir, TimelineView)> {
        let jj_bin = crate::vcs::jj::resolve_jj_bin()?;
        let dir = tempfile::tempdir().ok()?;
        let path = dir.path();

        let git_ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(path)
            .status()
            .ok()?
            .success();
        if !git_ok {
            return None;
        }
        // `jj git init --colocate` creates the repo `JjRepo::detect` needs;
        // unlike every later jj call it can't take `-R <path>` (that flag
        // targets a repo that already exists), so this one runs with
        // `current_dir` instead — the same split `crate::vcs::jj`'s own
        // fixture makes for the identical reason.
        let init_ok = std::process::Command::new(&jj_bin)
            .args([
                "--color",
                "never",
                "--no-pager",
                "git",
                "init",
                "--colocate",
            ])
            .current_dir(path)
            .status()
            .ok()?
            .success();
        if !init_ok {
            return None;
        }

        let repo = JjRepo::detect(path, jj_bin)?;
        std::fs::write(path.join("a.txt"), "one\n").ok()?;
        repo.snapshot().ok()?;
        std::fs::write(path.join("a.txt"), "one\nSYMONE SYMTWO\n").ok()?;
        repo.snapshot().ok()?;

        let view = TimelineView::new(repo, DEFAULT_OP_LOG_LIMIT).ok()?;
        Some((dir, view))
    }

    #[test]
    fn focus_next_pane_and_focus_prev_pane_toggle_list_and_diff_focus() {
        let Some((_dir, mut view)) = jj_timeline_fixture() else {
            eprintln!("skipping: jj not on PATH");
            return;
        };

        assert_eq!(view.focus, Focus::List);
        view.update(Action::FocusNextPane);
        assert_eq!(view.focus, Focus::Diff);
        // Only two panes exist, so a second forward step wraps straight
        // back to `List` — the same wraparound `cycle_focus`'s own tests
        // cover generically, observed here through a real view.
        view.update(Action::FocusNextPane);
        assert_eq!(view.focus, Focus::List);
        view.update(Action::FocusPrevPane);
        assert_eq!(view.focus, Focus::Diff);
        view.update(Action::FocusPrevPane);
        assert_eq!(view.focus, Focus::List);
    }

    /// Pins the behavior fix issue #13's split enables (see [`Focus`]'s
    /// docs): once the diff pane has focus, `NextSymbol` (vim's `l`, or
    /// emacs's `M-f` — Tab no longer means this) reaches the nested
    /// `App::update` and moves its `active_symbol`, instead of being
    /// swallowed by `TimelineView`'s own focus toggle the way every
    /// pre-#13 Tab press was.
    #[test]
    fn next_symbol_reaches_the_nested_diffs_cycle_symbol_once_the_diff_pane_is_focused() {
        let Some((_dir, mut view)) = jj_timeline_fixture() else {
            eprintln!("skipping: jj not on PATH");
            return;
        };

        view.update(Action::FocusNextPane);
        assert_eq!(view.focus, Focus::Diff);
        // `Bottom` lands on the trailing fold-gap row this two-line file's
        // diff renders below its last real content line (see
        // `RenderRow::Gap`) — one `CursorUp` from there reaches the
        // fixture's actual last line, the `SYMONE SYMTWO` addition, which
        // is what needs a `cycle_symbol`-able row under the cursor.
        view.update(Action::Bottom);
        view.update(Action::CursorUp);
        assert_eq!(view.diff_app.active_symbol, 0);

        view.update(Action::NextSymbol);
        assert_eq!(
            view.diff_app.active_symbol, 1,
            "NextSymbol must move the nested diff's active symbol, not \
             bounce focus back to List the way it did before issue #13"
        );
    }

    // ---- scroll_list_by / scroll_diff_by (issue #20 wheel routing) ----

    #[test]
    fn scroll_list_by_moves_selection_and_reloads_the_diff_once() {
        let Some((_dir, mut view)) = jj_timeline_fixture() else {
            eprintln!("skipping: jj not on PATH");
            return;
        };
        assert_eq!(view.selected, 0);
        assert_eq!(view.diff_note, None, "the newest snapshot has a real diff");

        view.scroll_list_by(1);
        assert_eq!(view.selected, 1, "moved to the older (only other) snapshot");
        assert_eq!(
            view.diff_note.as_deref(),
            Some("oldest loaded snapshot — no earlier one to diff against"),
            "selecting the oldest snapshot must have reloaded the diff \
             exactly once — this note only ever comes from `reload_diff`"
        );
        assert_eq!(
            view.focus,
            Focus::List,
            "wheel scrolling never touches keyboard focus"
        );
    }

    #[test]
    fn scroll_list_by_clamps_at_both_ends() {
        let Some((_dir, mut view)) = jj_timeline_fixture() else {
            eprintln!("skipping: jj not on PATH");
            return;
        };
        view.scroll_list_by(-100);
        assert_eq!(view.selected, 0);
        view.scroll_list_by(100);
        assert_eq!(view.selected, view.ops.len() - 1);
        let settled = view.selected;
        view.scroll_list_by(100);
        assert_eq!(view.selected, settled, "already at the bottom");
    }

    #[test]
    fn scroll_diff_by_moves_the_nested_diffs_viewport_without_selection_or_focus() {
        let Some((_dir, mut view)) = jj_timeline_fixture() else {
            eprintln!("skipping: jj not on PATH");
            return;
        };
        view.diff_app.set_viewport_height(1);
        view.diff_app.scroll_by(1000); // park it away from the top first
        let offset_before = view.diff_app.scroll_offset;
        let selected_before = view.selected;

        view.scroll_diff_by(-1);
        assert_ne!(view.diff_app.scroll_offset, offset_before);
        assert_eq!(
            view.selected, selected_before,
            "scrolling the nested diff must never move the list's own selection"
        );
        assert_eq!(view.focus, Focus::List, "and never touches focus either");
    }

    // ---- render_list windowing ------------------------------------------

    #[test]
    fn render_list_windows_around_a_selection_past_the_fold() {
        use ratatui::backend::TestBackend;

        let Some((_dir, mut view)) = jj_timeline_fixture() else {
            eprintln!("skipping: jj not on PATH");
            return;
        };
        // Overwrite the fixture's real (two-entry) history with a long
        // synthetic one so a small terminal genuinely can't show it all —
        // the actual jj plumbing above only exists to produce a real
        // `JjRepo`; the list itself renders from `view.ops` alone.
        view.ops = (0..50)
            .map(|i| op(&format!("op{i:02}"), i as i64))
            .collect();
        view.selected = 45;

        let backend = TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_list(frame, frame.area(), &view))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains("op45"),
            "the selected row must stay visible past the fold, not scroll \
             off the top of an unwindowed list:\n{text}"
        );
        assert!(
            !text.contains("op00"),
            "and the window must actually have moved, not just grown:\n{text}"
        );
    }
}
