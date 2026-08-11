//! Issue #20: safe mouse-capture lifecycle support — a per-frame geometry
//! snapshot and the wheel router built on top of it.
//!
//! [`FrameGeometry`] is rebuilt from scratch every frame by [`super::draw`]:
//! each render function records the exact [`Rect`] it just drew into,
//! tagged with a [`ScrollTarget`] naming what a wheel event landing inside
//! it should scroll. There is deliberately no second collection of
//! hardcoded sidebar widths/borders/overlay rectangles here — every
//! recorded rect is the *same* one the frame's real layout calculation
//! already produced, reused rather than re-derived (see the issue's
//! "Architecture guidance"). Recording happens in real draw order, which is
//! also drawing (and therefore modal) precedence: an overlay drawn on top
//! of the diff pane is recorded *after* it, so [`FrameGeometry::hit`]'s
//! last-match-wins scan finds the overlay first without a second priority
//! table to keep in sync.
//!
//! Wheel routing only ever needed pane *containment*; issue #21 is the
//! first caller that needs an actual row within one of these rects (which
//! changed-file tree row a click landed on), so [`FrameGeometry::hit_rect`]
//! hands back the recorded [`Rect`] itself alongside its target rather than
//! just the target [`FrameGeometry::hit`] returns — [`files_row_at`] turns
//! that rect plus the click point into a `visible_rows` index. Issue #22
//! (code-pane clicks) does the same against `ScrollTarget::DiffPane`/
//! `FilePane`, but a diff/file row can wrap, pair up side-by-side, or stand
//! for a header/gap/comment block — a single `Rect` plus a row count isn't
//! enough to map a click back to a logical row and display column, so it
//! adds a second, parallel piece of per-frame state: [`FrameGeometry::diff_content`]/
//! [`file_content`], a content [`Rect`] paired with a [`HitRow`] per
//! rendered terminal row, built by the *same* render loops that push
//! [`ratatui::text::Line`]s (`diff_view::render_unified`/`render_side_by_side`,
//! `file_view::render`) rather than a second wrap/layout algorithm re-deriving
//! the same rows here — see [`HitRow`]'s own docs.

use crate::keymap::Keymap;
use crate::ui::context_menu::MenuTarget;
use crate::ui::diff_view;
use crate::ui::file_tree::VisibleKind;
use crate::ui::file_view;
use crate::ui::help::{self, HelpState};
use crate::ui::hover_popup::HoverState;
use crate::ui::navigation::{FilesConfirmOutcome, JumpStack, record_jump};
use crate::ui::pane;
use crate::ui::refs_panel::RefsPanel;
use crate::ui::units_panel::UnitsPanel;
use crate::ui::view::{View, ViewStack};
use ratatui::layout::{Position, Rect};

/// How many logical rows one wheel "click" scrolls — the issue's
/// recommended "small stable logical amount," applied uniformly to every
/// target this module routes to regardless of that pane's own row height.
/// `ui::mod`'s `Event::Mouse` arm negates this for `ScrollUp`.
pub const WHEEL_SCROLL_ROWS: isize = 3;

/// Which pane/overlay a recorded [`Rect`] belongs to — the scroll router's
/// entire vocabulary of "what's under the pointer." One variant per
/// scrollable surface the issue's req 6 lists; anything not covered here
/// (a non-scrollable popup like the scope menu or the units-setup wizard)
/// simply never records a rect at all, so a wheel over it is a no-op by
/// construction rather than something this enum has to represent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollTarget {
    DiffFiles,
    DiffPane,
    FilePane,
    TimelineList,
    TimelineDiff,
    LogList,
    InspectorServers,
    InspectorDetail,
    InspectorJournal,
    HoverPopup,
    RefsPanel,
    UnitsPanel,
    HelpPopup,
    /// Fully-modal overlays with no scrollable content of their own (the
    /// scope menu, the units-setup wizard, the compose editor). Recorded
    /// over the whole content area they block input to — not just their
    /// own popup rectangle — so a wheel tick anywhere over the pane
    /// beneath them is captured and discarded rather than scrolling
    /// content the reviewer can't currently interact with (issue #20 req
    /// 7's modal precedence). Three named variants rather than one shared
    /// "modal" tag because click routing (issues #22/#23) treats them
    /// differently: a stray click may close a scope menu but must never
    /// discard a compose buffer.
    ScopeMenuModal,
    UnitsSetupModal,
    ComposeModal,
}

/// One rendered *visual* row's mapping back to its logical source line:
/// which row of `App::rows` (or `FileView`'s own 0-based line index) it
/// belongs to, and the display column (in that logical line's tab-expanded
/// column space — the same space [`crate::diff::ColumnMap`]/
/// [`crate::ui::symbols::scan`] use) the first rendered character of *this*
/// visual row starts at. Row 0 of an unwrapped line, or the first visual row
/// of a wrapped one, always has `content_start_col == 0`; a continuation row
/// carries however many columns of the logical line already wrapped away
/// above it — exactly `content_line`'s own `col_offset` accumulator (see
/// `diff_view::content_line`/`file_view::content_line`), which is what
/// produces this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineHit {
    pub row_idx: usize,
    pub content_start_col: usize,
}

/// One rendered terminal row's hit-test tag, alongside its [`ratatui::text::Line`]
/// in the `Vec` a content render loop builds — the vocabulary #22's
/// "architecture guidance" asks for: every row a click could land on, kept
/// distinct enough that [`resolve_hit`] never has to guess what a row
/// *means* from its geometry alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitRow {
    /// A content line in the unified layout — the only layout where a
    /// [`crate::diff::RenderRow::Line`] produces a row of its own, rather
    /// than being folded into a [`Self::SideBySide`] cell (see
    /// `crate::diff::flatten_side_by_side`'s docs).
    Unified(LineHit),
    /// One visual row of a [`crate::diff::SideBySideRow::Paired`] pair.
    /// `left_width` is the divider's column within the pane's content rect
    /// (constant for the whole pane, not just this row) — [`resolve_hit`]
    /// needs it to decide which cell `local_x` fell in without a second
    /// layout computation. `old`/`new` are `None` for the blank filler a
    /// shorter side pads out with when the other side wrapped to more
    /// visual rows (see `diff_view::side_by_side_row_line`'s docs) — padding
    /// with nothing to hit-test, not a row this variant can resolve.
    SideBySide {
        left_width: usize,
        old: Option<LineHit>,
        new: Option<LineHit>,
    },
    /// A file/hunk header or binary notice — [`crate::diff::RenderRow::FileHeader`]/
    /// `BinaryNotice`/`HunkHeader`. Cursor-addressable (see `App::cursor`'s
    /// docs) but has no display column or symbol of its own to resolve.
    Structural { flat_idx: usize },
    /// A fold row ([`crate::diff::RenderRow::Gap`]) — cursor-addressable,
    /// same reasoning as [`Self::Structural`].
    Gap { flat_idx: usize },
    /// One line of an inline comment's rendered body block — maps back to
    /// the flat row the comment is anchored to (its range's *start*, for a
    /// multi-line range — see `diff_view::comments_starting_at_row`'s docs),
    /// never to a row/column of its own; clicking anywhere in the block
    /// positions the cursor on the anchor row.
    CommentBody { anchor_flat_idx: usize },
    /// A content line in [`crate::ui::file_view::FileView`]'s single-column
    /// layout — [`Self::Unified`]'s file-view counterpart; kept as a
    /// separate variant (rather than reusing `Unified`) so a caller can
    /// never accidentally feed a diff-pane hit through file-view resolution
    /// or vice versa.
    FileLine(LineHit),
}

/// One click's column resolved against a single [`LineHit`]: `(display_col,
/// content_click)`. `content_click` is `false` when `local_x` lands in the
/// row's gutter (`local_x < gutter_width`) — a position-only click (issue
/// #22 req 3) — in which case `display_col` is `hit.content_start_col`
/// (this visual row's own first column) rather than a value derived from
/// `local_x`, since there's no on-content `local_x` to derive it from.
/// `gutter_width` is the *same* constant for a row's first visual row and
/// every continuation row after it (`diff_view::continuation_gutter`'s doc
/// guarantees this — see this module's own doc on why that means one
/// subtraction here, never a second per-row-kind width table).
fn resolve_line_column(hit: LineHit, gutter_width: usize, local_x: u16) -> (usize, bool) {
    let x = local_x as usize;
    if x < gutter_width {
        (hit.content_start_col, false)
    } else {
        (hit.content_start_col + (x - gutter_width), true)
    }
}

/// A click resolved against one recorded [`HitRow`]: which logical row to
/// move the cursor to, the display column to resolve the active symbol
/// against, whether this was an on-content click at all (`false` for a
/// gutter click or a structural/gap/comment-body row — position only, never
/// an identifier hit regardless of what `display_col` happens to overlap),
/// and whether this side is even eligible for go-to-definition
/// (`new_or_unified_side`, `false` only for a side-by-side *old* cell — see
/// its own docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedClick {
    pub row_idx: usize,
    pub display_col: usize,
    pub content_click: bool,
    pub new_or_unified_side: bool,
}

/// Resolves a click at local content-rect column `local_x` against one
/// already-looked-up [`HitRow`] — the row lookup itself (`local_y` indexing
/// into a frame's `Vec<HitRow>`, out-of-range past the last rendered row)
/// is [`FrameGeometry::diff_row_hit`]/[`file_row_hit`]'s job, not this
/// function's; this is the pure per-row half, independently testable
/// against a hand-built [`HitRow`] with no [`FrameGeometry`]/[`Rect`] in the
/// picture at all. `None` for a side-by-side divider column, or a
/// side-by-side cell whose [`LineHit`] is itself `None` (blank filler — see
/// [`HitRow::SideBySide`]'s docs) — both "nothing here to hit," not a row
/// this function can resolve *and* say "position only" about, unlike a
/// gutter click (see [`ResolvedClick::content_click`]).
///
/// No display-width walk of its own anywhere in here — every column
/// decision is a plain subtraction against `content_start_col`/`left_width`,
/// both already computed by the render loop that produced `hit` in the
/// line's own tab-expanded, grapheme-aware display-column space. That's the
/// "no second wrap/layout algorithm" the issue's architecture guidance asks
/// for (see this module's own doc comment).
pub fn resolve_hit(hit: HitRow, gutter_width: usize, local_x: u16) -> Option<ResolvedClick> {
    Some(match hit {
        HitRow::Unified(line) | HitRow::FileLine(line) => {
            let (display_col, content_click) = resolve_line_column(line, gutter_width, local_x);
            ResolvedClick {
                row_idx: line.row_idx,
                display_col,
                content_click,
                new_or_unified_side: true,
            }
        }
        HitRow::SideBySide {
            left_width,
            old,
            new,
        } => {
            let x = local_x as usize;
            if x < left_width {
                let line = old?;
                let (display_col, content_click) = resolve_line_column(line, gutter_width, local_x);
                ResolvedClick {
                    row_idx: line.row_idx,
                    display_col,
                    content_click,
                    new_or_unified_side: false,
                }
            } else if x == left_width {
                return None; // the divider itself — non-actionable (req 7)
            } else {
                let line = new?;
                // Shift past the left column and the one-column divider
                // before re-running the same gutter subtraction the left
                // cell just used — the new cell's own `local_x` space starts
                // fresh at its own gutter, exactly as if it were rendered on
                // its own (see `side_by_side_row_line`'s docs on how the two
                // cells' spans get concatenated with a divider between).
                let shifted = local_x - (left_width as u16 + 1);
                let (display_col, content_click) = resolve_line_column(line, gutter_width, shifted);
                ResolvedClick {
                    row_idx: line.row_idx,
                    display_col,
                    content_click,
                    new_or_unified_side: true,
                }
            }
        }
        // Neither carries a column to resolve — see `HitRow::Structural`/
        // `Gap`/`CommentBody`'s own docs — so `display_col` is a placeholder
        // `0` an `App`/`FileView` positioning call ignores anyway (neither
        // has a symbol to select on a header/gap/comment-body row: see
        // `App::cursor_row_text`'s `None` fallback for anything but a
        // `RenderRow::Line`). `new_or_unified_side: false` keeps these out
        // of identifier-hit consideration unconditionally, on top of
        // `content_click: false` already doing so — belt and suspenders
        // against a future caller that only checks one of the two.
        HitRow::Structural { flat_idx } | HitRow::Gap { flat_idx } => ResolvedClick {
            row_idx: flat_idx,
            display_col: 0,
            content_click: false,
            new_or_unified_side: false,
        },
        HitRow::CommentBody { anchor_flat_idx } => ResolvedClick {
            row_idx: anchor_flat_idx,
            display_col: 0,
            content_click: false,
            new_or_unified_side: false,
        },
    })
}

/// One frame's worth of "what's drawn where," rebuilt fresh every render —
/// see this module's docs for why a fresh build beats caching across
/// frames (every rect can move: resize, a toggled sidebar, a popup opening).
#[derive(Debug, Default)]
pub struct FrameGeometry {
    /// Append-only, in draw order. A `Vec` rather than anything spatially
    /// indexed: a frame has on the order of ten recorded rects at most, so
    /// a linear reverse scan in [`Self::hit`] is simpler than any tree and
    /// costs nothing measurable.
    entries: Vec<(Rect, ScrollTarget)>,
    /// Issue #22: the main diff pane's *content* rect (inside its border —
    /// see `diff_view::render_focusable`'s `PaneChrome` — never the outer
    /// rect [`ScrollTarget::DiffPane`] records for wheel containment) paired
    /// with one [`HitRow`] per rendered terminal row within it. `None` when
    /// nothing was drawn to record against (`View::Diff` isn't on top this
    /// frame) — kept as a field parallel to `entries`, not folded into it,
    /// since a `HitRow` needs a `local_y`-indexed `Vec` alongside its rect,
    /// not just a target tag (see [`resolve_hit`]). A single `Option` rather
    /// than something keyed by view assumes one top-level content pane needs
    /// hit-testing per frame — true for both `View::Diff` and `View::File`
    /// today (they're never both on top at once), flagged here for whoever
    /// adds a second concurrently-hit-testable pane later.
    diff_content: Option<(Rect, Vec<HitRow>)>,
    /// As [`Self::diff_content`], for `View::File`'s single content pane.
    file_content: Option<(Rect, Vec<HitRow>)>,
    /// Issue #23: the context menu's own popup rect, recorded outside
    /// `entries` entirely (unlike every `ScrollTarget` above) — a right-
    /// click that misses this rect must still resolve against whatever's
    /// really underneath (`DiffFiles`/`DiffPane`/`FilePane`, the open
    /// flow's retarget rule, req 7), which a coarse "block the whole pane"
    /// recording the way the three `*Modal` variants use would defeat:
    /// [`Self::hit_rect`]'s last-match-wins scan would then find the block
    /// itself first, and there'd be no way to see past it to what a
    /// retarget needs to resolve. `mouse::handle_right_click` checks this
    /// field directly, ahead of any `hit_rect` lookup at all; wheel routing
    /// is blocked the same way — an explicit `context_menu.is_none()` guard
    /// in `ui::mod`'s event loop, not a recorded rect here.
    context_menu_rect: Option<Rect>,
}

impl FrameGeometry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the diff pane's content rect and per-row [`HitRow`]s for this
    /// frame — called once from `draw`'s `View::Diff` arm, after
    /// `diff_view::render_focusable` has both drawn the content and handed
    /// back the exact `HitRow`s it drew alongside (see this module's doc
    /// comment on why the render loop itself is the one source of this,
    /// never a second computation here).
    pub fn record_diff_content(&mut self, rect: Rect, hits: Vec<HitRow>) {
        self.diff_content = Some((rect, hits));
    }

    /// As [`Self::record_diff_content`], for `View::File`.
    pub fn record_file_content(&mut self, rect: Rect, hits: Vec<HitRow>) {
        self.file_content = Some((rect, hits));
    }

    /// Records the context menu's own precise popup rect for this frame —
    /// see [`Self::context_menu_rect`]'s field docs.
    pub fn record_context_menu(&mut self, rect: Rect) {
        self.context_menu_rect = Some(rect);
    }

    /// The context menu's popup rect this frame, or `None` when no menu was
    /// drawn — what [`crate::ui::context_menu::entry_at`] resolves a click
    /// against.
    pub fn context_menu_rect(&self) -> Option<Rect> {
        self.context_menu_rect
    }

    /// `(local_x, local_y, hit)` for a click at `(col, row)` landing inside
    /// the diff pane's recorded content rect this frame, on a row `hits`
    /// actually has an entry for — `None` outside the rect, past the last
    /// rendered row (a click into the content rect's own blank remainder),
    /// or when nothing was recorded at all this frame (see
    /// [`Self::diff_content`]'s docs). `local_x`/`local_y` are `(col, row)`
    /// translated into the content rect's own coordinate space (`0` at the
    /// rect's own top-left) — what [`resolve_hit`] expects as its own
    /// `local_x`, alongside the `hit` this already looked up for the
    /// caller so `resolve_hit` itself never needs the whole `Vec<HitRow>`
    /// or a row index to search it with.
    pub fn diff_row_hit(&self, col: u16, row: u16) -> Option<(u16, u16, &HitRow)> {
        content_row_hit(self.diff_content.as_ref(), col, row)
    }

    /// As [`Self::diff_row_hit`], for `View::File`'s content rect.
    pub fn file_row_hit(&self, col: u16, row: u16) -> Option<(u16, u16, &HitRow)> {
        content_row_hit(self.file_content.as_ref(), col, row)
    }

    /// Records `rect` as belonging to `target` — a no-op for a zero-width
    /// or zero-height rect, which a collapsed/hidden pane can legitimately
    /// produce (e.g. a sidebar rect when it's toggled off never even
    /// reaches here, but a defensive check costs nothing and means a
    /// degenerate rect from any future caller can never become a hittable
    /// point with no visible pixels behind it).
    pub fn record(&mut self, rect: Rect, target: ScrollTarget) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.entries.push((rect, target));
    }

    /// As [`Self::hit`], but also returns the exact [`Rect`] that matched —
    /// issue #21's [`files_row_at`] needs the rect itself (to derive a row
    /// index within it), not just which target it belongs to. `hit` is a
    /// thin wrapper over this rather than the other way around, so wheel
    /// routing's existing single-value return never had to change.
    pub fn hit_rect(&self, col: u16, row: u16) -> Option<(Rect, ScrollTarget)> {
        let point = Position { x: col, y: row };
        self.entries
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(point))
            .copied()
    }

    /// The target whose rect contains `(col, row)`, scanning most- to
    /// least-recently recorded — i.e. reverse draw order. Since draw order
    /// *is* modal precedence (see this module's docs), this is the entire
    /// precedence rule: the last thing drawn on top of a point is what a
    /// wheel event there scrolls, with no separate table of overlay
    /// z-order to keep in sync with `draw`'s own call order.
    pub fn hit(&self, col: u16, row: u16) -> Option<ScrollTarget> {
        self.hit_rect(col, row).map(|(_, target)| target)
    }
}

/// Shared body of [`FrameGeometry::diff_row_hit`]/[`FrameGeometry::file_row_hit`]:
/// `(col, row)` translated into `content`'s own coordinate space and handed
/// back alongside the `HitRow` at that local row, or `None` outside the
/// rect (or when `content` itself is `None` — nothing recorded this frame).
/// Column translation only, never row-index resolution: `hits[local_y]`
/// (via direct indexing, not [`resolve_hit`]) is the row itself, since a
/// caller needing the *resolved* click also needs `local_x`, which this
/// returns raw for [`resolve_hit`] to consume — folding the two together
/// would mean this function taking a `gutter_width` it has no other use
/// for.
fn content_row_hit(
    content: Option<&(Rect, Vec<HitRow>)>,
    col: u16,
    row: u16,
) -> Option<(u16, u16, &HitRow)> {
    let (rect, hits) = content?;
    if !rect.contains(Position { x: col, y: row }) {
        return None;
    }
    let local_x = col - rect.x;
    let local_y = row - rect.y;
    hits.get(local_y as usize)
        .map(|hit| (local_x, local_y, hit))
}

/// Which changed-files tree row — an index into `App::visible_rows` — sits
/// under `(col, row)`, given `outer`: the same *outer*, border-inclusive
/// [`Rect`] `draw` recorded under [`ScrollTarget::DiffFiles`] (see this
/// module's `KEY ASSUMPTION` in plan-21's grounding: `sidebar::render`
/// computes its own inner rect off exactly this rect via `Block::inner`, so
/// deriving it again here with [`pane::inner_rect`] — the same border-math
/// function, not a hand-counted literal — reproduces it exactly rather than
/// risking drift between two independent border calculations). Excludes
/// border/title/hint rows by simple non-containment (req 3) rather than
/// listing them, and blank space below the last row by comparing the
/// resulting index against `visible_row_count` (req 5) — `sidebar::render`
/// never wraps a row (one [`crate::ui::file_tree::VisibleRow`] is always
/// exactly one terminal line, no `Paragraph::wrap`), so `row - inner.y` is
/// already a row *count* to add to the scroll offset, not a display
/// position that needs unwrapping first.
pub fn files_row_at(
    outer: Rect,
    scroll_offset: usize,
    visible_row_count: usize,
    col: u16,
    row: u16,
) -> Option<usize> {
    let inner = pane::inner_rect(outer.width, outer);
    if !inner.contains(Position { x: col, y: row }) {
        return None;
    }
    let idx = scroll_offset + (row - inner.y) as usize;
    (idx < visible_row_count).then_some(idx)
}

/// A primary click at `(col, row)`. `shift` is the SGR mouse report's own
/// shift-modifier bit (issue #20 req 4's `Cb` decode, forwarded straight
/// from crossterm's `MouseEvent::modifiers`) — issue #22 req 4's
/// shift-click-extends-selection-never-definition rule.
///
/// [`FrameGeometry::hit_rect`]'s last-drawn-wins scan is the *entire*
/// dispatch here: which arm below runs is decided purely by whichever
/// [`ScrollTarget`] the click's `(col, row)` resolves to, so an overlay
/// drawn on top of the diff/file pane (a hover popup, the references/units
/// panel, help, a fully-modal scope-menu/units-setup/compose rect) already
/// wins that scan and never reaches [`handle_diff_pane_click`]/
/// [`handle_file_pane_click`] at all (req 8) — no second "is something on
/// top of this point" check needed here. Anything besides
/// [`ScrollTarget::DiffFiles`]/[`ScrollTarget::DiffPane`]/`FilePane` is a
/// no-op by construction: #23 owns richer overlay-click behavior (dismiss,
/// context menus), so a click landing on one of those today simply does
/// nothing rather than guessing at behavior this issue doesn't own.
///
/// Returns whether the click landed on an eligible identifier — `true`
/// means the caller (`ui::mod`'s `Event::Mouse` arm) should now dispatch
/// `Action::GotoDefinition` through the ordinary `handle_action` pipeline,
/// exactly as keyboard `gd` would, so readiness/supersession/jump-history/
/// status stay single-sourced (see this module's own doc comment on why
/// that dispatch happens one level up rather than being threaded through
/// here — `handle_action` needs roughly twenty pieces of event-loop state
/// this function has no reason to also take).
pub fn handle_left_click(
    geometry: &FrameGeometry,
    stack: &mut ViewStack,
    jump_stack: &mut JumpStack,
    hover_state: &mut HoverState,
    col: u16,
    row: u16,
    shift: bool,
) -> bool {
    match geometry.hit_rect(col, row) {
        Some((rect, ScrollTarget::DiffFiles)) => {
            handle_files_click(rect, stack, jump_stack, hover_state, col, row);
            false
        }
        Some((_, ScrollTarget::DiffPane)) => {
            handle_diff_pane_click(geometry, stack, jump_stack, hover_state, col, row, shift)
        }
        Some((_, ScrollTarget::FilePane)) => {
            handle_file_pane_click(geometry, stack, jump_stack, hover_state, col, row, shift)
        }
        _ => false,
    }
}

/// `from` is read *before* the click can move anything — mirroring
/// `ui::mod`'s `Action::Confirm`-while-`Files` arm, whose `from`-then-
/// mutate ordering this is the mouse equivalent of — so a valid prior
/// source location still round-trips through `Ctrl-o` afterward, and
/// `record_jump`'s own equality check quietly declines to record a
/// same-position "jump" (clicking the row the cursor's already on).
fn handle_files_click(
    rect: Rect,
    stack: &mut ViewStack,
    jump_stack: &mut JumpStack,
    hover_state: &mut HoverState,
    col: u16,
    row: u16,
) {
    let Some(idx) = (match stack.top() {
        View::Diff(app) => files_row_at(
            rect,
            app.files_scroll_offset,
            app.visible_rows.len(),
            col,
            row,
        ),
        View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => None,
    }) else {
        return;
    };
    let from = stack.top().jump_entry();
    let outcome = match stack.top_mut() {
        View::Diff(app) => app.click_files_row(idx),
        // Unreachable in practice — `files_row_at` above only produced
        // `Some` inside the `View::Diff` arm — but `stack.top_mut()` is a
        // second, independent lookup, so this stays exhaustive rather than
        // assuming the view didn't change between the two calls.
        View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => return,
    };
    if let FilesConfirmOutcome::Opened(to) = outcome {
        record_jump(jump_stack, from, Some(to));
        hover_state.invalidate();
    }
}

/// Issue #22's code-pane click: resolves `(col, row)` against the diff
/// pane's recorded [`HitRow`]s, positions the cursor the same way keyboard
/// navigation would (`before`/`from`/`record_jump` bracketing mirrors the
/// `Action::NextDiagnostic`/`NextMatch` arms in `ui::mod::handle_action` —
/// see those for the same shape), and reports whether the click should
/// chase go-to-definition.
///
/// `shift` blocks identifier-hit unconditionally (req 4: shift-click only
/// ever extends the existing visual selection — riding
/// `position_cursor_from_click`'s ordinary cursor movement, since
/// `App::visual_bounds` already recomputes from the cursor on every call —
/// never definition, even when the click actually lands on a symbol).
/// `resolved.content_click` blocks it for a gutter click, and
/// `resolved.new_or_unified_side` blocks it for a side-by-side *old* cell —
/// both position-only per req 3. A plain identifier click ends an active
/// visual selection first (issue #16's selection-invalidation rule) — a
/// click-path-only quirk keyboard `gd` doesn't share; see `App::cancel_visual`'s
/// docs and this issue's own accepted-decisions note on the deliberate
/// asymmetry.
fn handle_diff_pane_click(
    geometry: &FrameGeometry,
    stack: &mut ViewStack,
    jump_stack: &mut JumpStack,
    hover_state: &mut HoverState,
    col: u16,
    row: u16,
    shift: bool,
) -> bool {
    let Some((local_x, _local_y, hit)) = geometry.diff_row_hit(col, row) else {
        return false;
    };
    let Some(resolved) = resolve_hit(*hit, diff_view::gutter_width(), local_x) else {
        return false;
    };

    let before_hover = stack.top().hover_cursor_key();
    let from = stack.top().jump_entry();
    let matched = match stack.top_mut() {
        View::Diff(app) => app.position_cursor_from_click(resolved.row_idx, resolved.display_col),
        // `ScrollTarget::DiffPane`/`diff_content` are only ever recorded
        // from `draw`'s `View::Diff` arm — unreachable in practice, same
        // defensiveness as `handle_files_click`'s identical `View::File`
        // fallback above.
        View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => return false,
    };
    if stack.top().hover_cursor_key() != before_hover {
        hover_state.invalidate();
    }
    let identifier_hit =
        !shift && resolved.content_click && resolved.new_or_unified_side && matched;
    // An identifier click deliberately skips the positioning record: the
    // definition flow it's about to dispatch records the just-clicked
    // position as its own origin (via `navigate_to`), so recording here
    // too would give the same click two history entries where keyboard
    // `gd` produces one — the acceptance criterion is "exactly like
    // keyboard gd." A non-symbol click records normally (mouse-triggered
    // positioning is a significant jump per the epic's decision 2).
    if !identifier_hit {
        record_jump(jump_stack, from, stack.top().jump_entry());
    }
    if identifier_hit && let View::Diff(app) = stack.top_mut() {
        // A no-op (returns `false`, touches nothing) when visual mode isn't
        // active — safe to call unconditionally rather than checking
        // `visual_active()` first.
        app.cancel_visual();
    }
    identifier_hit
}

/// As [`handle_diff_pane_click`], for `View::File` — no visual-selection
/// concept to cancel (see `FileView::update`'s `ToggleVisualLine` no-op
/// arm), so `shift` here only ever suppresses identifier-hit; there is
/// nothing else for it to extend.
fn handle_file_pane_click(
    geometry: &FrameGeometry,
    stack: &mut ViewStack,
    jump_stack: &mut JumpStack,
    hover_state: &mut HoverState,
    col: u16,
    row: u16,
    shift: bool,
) -> bool {
    let Some((local_x, _local_y, hit)) = geometry.file_row_hit(col, row) else {
        return false;
    };
    let Some(resolved) = resolve_hit(*hit, file_view::gutter_width(), local_x) else {
        return false;
    };

    let before_hover = stack.top().hover_cursor_key();
    let from = stack.top().jump_entry();
    let matched = match stack.top_mut() {
        View::File(file) => file.position_cursor_from_click(resolved.row_idx, resolved.display_col),
        View::Diff(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => return false,
    };
    if stack.top().hover_cursor_key() != before_hover {
        hover_state.invalidate();
    }
    let identifier_hit =
        !shift && resolved.content_click && resolved.new_or_unified_side && matched;
    // Same one-entry-per-click rule as `handle_diff_pane_click` — see the
    // doc there.
    if !identifier_hit {
        record_jump(jump_stack, from, stack.top().jump_entry());
    }
    identifier_hit
}

/// What a right-click resolved to (issue #23) — `ui::mod`'s open/retarget/
/// close flow turns this into an actual
/// [`crate::ui::context_menu::ContextMenuState`]. Deriving the menu's real
/// *entries* needs `LspManager`/`App::comment_target`/... none of which
/// this module depends on (see this module's own doc comment on why
/// `Action::GotoDefinition` dispatch is likewise one level up from
/// [`handle_left_click`]) — this only resolves *what* was clicked and
/// positions the cursor/selection accordingly, exactly as
/// [`handle_left_click`] already does for the left button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RightClickOutcome {
    /// The click landed on the open menu's own popup rect, a fully-modal
    /// overlay (compose/help/the scope-menu/units-setup wizard), or
    /// nothing recorded at all (a view — Timeline/Log/the LSP inspector —
    /// this menu was never wired up for). A true no-op: nothing changes,
    /// including whether a menu is open — a fully-modal overlay's content
    /// must never be disturbed by a stray click regardless of which mouse
    /// button, and a second right-click on the menu itself has nothing to
    /// do (left-click is what invokes an entry).
    Noop,
    /// The click closed a dismissable overlay that would otherwise
    /// visually overlap a menu opening underneath/beside it (req 9) — the
    /// hover popup specifically, already closed by this call (this
    /// function holds `&mut HoverState` already, for the ordinary
    /// cursor-move invalidation below). No menu opens from this same
    /// click.
    ClosedHover,
    /// As [`Self::ClosedHover`], for the references/units panel — reported
    /// rather than closed here, since neither is owned by this module (see
    /// `ui::mod`'s `RefsPanelState`); the caller clears both the same
    /// unconditional way its own left-click dismiss guard already does.
    ClosedPanel,
    /// A real, menu-eligible target — the caller derives entries and
    /// opens/retargets the menu at the click point.
    Target(MenuTarget),
    /// Blank space, a pane border, or a click past the end of the files
    /// tree — the caller closes an already-open menu (retarget-when-valid/
    /// close-when-invalid, req 7) and does nothing otherwise.
    Miss,
}

/// A right-click at `(col, row)` (issue #23). Positions the cursor/
/// selection for whatever real target it resolves to exactly like
/// [`handle_left_click`] would — `App::position_cursor_from_click`/
/// `App::select_files_row`/`FileView::position_cursor_from_click` — but
/// deliberately *never* the identifier-click-chases-`gd` follow-up
/// [`handle_left_click`] triggers, nor `App::cancel_visual`: resolving a
/// menu target must never navigate anywhere or discard an active visual
/// selection out from under the very "Add comment (N lines)"/"Yank
/// selection" entries that selection is about to make available.
///
/// Checks [`FrameGeometry::context_menu_rect`] *before* any `hit_rect`
/// lookup, deliberately outside the `match` below: an already-open menu's
/// popup has no `ScrollTarget` of its own to match against (see that
/// field's docs on why), and checking it first is what lets a right-click
/// elsewhere in the very pane the menu is drawn over still resolve against
/// whatever's really there — `DiffFiles`/`DiffPane`/`FilePane`, recorded
/// every frame regardless of whether a menu happens to be open — instead of
/// being swallowed by some coarse "the menu owns this whole pane" rect the
/// way a wheel tick is (see `ui::mod`'s own `context_menu.is_none()` guard
/// on its wheel arm). That is what makes retargeting (req 7) possible at
/// all: a second right-click on a *different* row must reach this
/// function's ordinary target resolution below, not stop at "a menu is
/// open, therefore no-op."
pub fn handle_right_click(
    geometry: &FrameGeometry,
    stack: &mut ViewStack,
    hover_state: &mut HoverState,
    col: u16,
    row: u16,
) -> RightClickOutcome {
    if geometry
        .context_menu_rect()
        .is_some_and(|rect| rect.contains(Position { x: col, y: row }))
    {
        return RightClickOutcome::Noop;
    }
    match geometry.hit_rect(col, row) {
        Some((
            _,
            ScrollTarget::ComposeModal
            | ScrollTarget::ScopeMenuModal
            | ScrollTarget::UnitsSetupModal
            | ScrollTarget::HelpPopup,
        )) => RightClickOutcome::Noop,
        Some((_, ScrollTarget::HoverPopup)) => {
            hover_state.close();
            RightClickOutcome::ClosedHover
        }
        Some((_, ScrollTarget::RefsPanel | ScrollTarget::UnitsPanel)) => {
            RightClickOutcome::ClosedPanel
        }
        Some((rect, ScrollTarget::DiffFiles)) => match stack.top_mut() {
            View::Diff(app) => {
                let Some(idx) = files_row_at(
                    rect,
                    app.files_scroll_offset,
                    app.visible_rows.len(),
                    col,
                    row,
                ) else {
                    return RightClickOutcome::Miss;
                };
                if !app.select_files_row(idx) {
                    return RightClickOutcome::Miss;
                }
                match app.visible_rows[idx].kind {
                    VisibleKind::Directory { .. } => {
                        RightClickOutcome::Target(MenuTarget::TreeDir {
                            path: app.visible_rows[idx].id.path.clone(),
                        })
                    }
                    VisibleKind::File { .. } => RightClickOutcome::Target(MenuTarget::TreeFile),
                }
            }
            View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => {
                RightClickOutcome::Miss
            }
        },
        Some((_, ScrollTarget::DiffPane)) => {
            diff_pane_menu_target(geometry, stack, hover_state, col, row)
        }
        Some((_, ScrollTarget::FilePane)) => {
            file_pane_menu_target(geometry, stack, hover_state, col, row)
        }
        // Every remaining target (the timeline/log lists, the inspector's
        // three panes) is a view this menu was never wired up for at all —
        // req 8's own scope, not a gap to fill here.
        _ => RightClickOutcome::Miss,
    }
}

/// [`handle_right_click`]'s `DiffPane` branch: resolves `(col, row)` the
/// same way [`handle_diff_pane_click`] does, positions the cursor via
/// `App::position_cursor_from_click`, invalidates a now-stale hover popup
/// on a cursor-key change (mirroring [`handle_diff_pane_click`]'s own
/// `before_hover` check) — but stops there, reporting
/// [`RightClickOutcome::Target`] rather than chasing go-to-definition or
/// cancelling an active visual selection (see [`handle_right_click`]'s own
/// docs).
fn diff_pane_menu_target(
    geometry: &FrameGeometry,
    stack: &mut ViewStack,
    hover_state: &mut HoverState,
    col: u16,
    row: u16,
) -> RightClickOutcome {
    let Some((local_x, _local_y, hit)) = geometry.diff_row_hit(col, row) else {
        return RightClickOutcome::Miss;
    };
    let Some(resolved) = resolve_hit(*hit, diff_view::gutter_width(), local_x) else {
        return RightClickOutcome::Miss;
    };
    let before_hover = stack.top().hover_cursor_key();
    match stack.top_mut() {
        View::Diff(app) => {
            app.position_cursor_from_click(resolved.row_idx, resolved.display_col);
        }
        View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => {
            return RightClickOutcome::Miss;
        }
    }
    if stack.top().hover_cursor_key() != before_hover {
        hover_state.invalidate();
    }
    RightClickOutcome::Target(MenuTarget::DiffRow)
}

/// As [`diff_pane_menu_target`], for `View::File`.
fn file_pane_menu_target(
    geometry: &FrameGeometry,
    stack: &mut ViewStack,
    hover_state: &mut HoverState,
    col: u16,
    row: u16,
) -> RightClickOutcome {
    let Some((local_x, _local_y, hit)) = geometry.file_row_hit(col, row) else {
        return RightClickOutcome::Miss;
    };
    let Some(resolved) = resolve_hit(*hit, file_view::gutter_width(), local_x) else {
        return RightClickOutcome::Miss;
    };
    let before_hover = stack.top().hover_cursor_key();
    match stack.top_mut() {
        View::File(file) => {
            file.position_cursor_from_click(resolved.row_idx, resolved.display_col);
        }
        View::Diff(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => {
            return RightClickOutcome::Miss;
        }
    }
    if stack.top().hover_cursor_key() != before_hover {
        hover_state.invalidate();
    }
    RightClickOutcome::Target(MenuTarget::FileViewRow)
}

#[allow(clippy::too_many_arguments)]
// one small dispatcher threading
// through every overlay a wheel event might land on — mirrors
// `super::handle_action`'s own justification: a struct would just move the
// same independently-borrowed pieces of event-loop state one level down.
/// Routes one wheel tick at `(col, row)` to whichever pane/overlay
/// [`FrameGeometry::hit`] says is there, moving it by `delta` logical rows
/// (negative scrolls up). A miss (nothing recorded under the pointer, e.g. a
/// resize race or the status bar) is silently ignored — there is nothing to
/// scroll and nothing to report.
///
/// Every arm calls exactly one small, view-owned method
/// (`App::scroll_by`/`scroll_files_by`, `FileView::scroll_by`, ...) and
/// never reaches into a pane's cursor/focus fields directly — by
/// construction, no arm below ever touches `stack`'s keyboard focus, which
/// is what makes req 5's "wheel scrolling does not steal keyboard focus"
/// true without a separate check: there is simply no code path here that
/// could change it.
pub fn scroll_at(
    geometry: &FrameGeometry,
    col: u16,
    row: u16,
    delta: isize,
    stack: &mut ViewStack,
    hover_state: &mut HoverState,
    refs_panel: Option<&mut RefsPanel>,
    units_panel: Option<&mut UnitsPanel>,
    help: Option<&mut HelpState>,
    keymap: &Keymap,
    help_row_count_cache: &mut Option<(String, usize)>,
    area: Rect,
) {
    let Some(target) = geometry.hit(col, row) else {
        return;
    };
    match target {
        ScrollTarget::DiffFiles => {
            if let View::Diff(app) = stack.top_mut() {
                app.scroll_files_by(delta);
            }
        }
        ScrollTarget::DiffPane => {
            if let View::Diff(app) = stack.top_mut() {
                app.scroll_by(delta);
            }
        }
        ScrollTarget::FilePane => {
            if let View::File(file) = stack.top_mut() {
                file.scroll_by(delta);
            }
        }
        ScrollTarget::TimelineList => {
            if let View::Timeline(timeline) = stack.top_mut() {
                timeline.scroll_list_by(delta);
            }
        }
        ScrollTarget::TimelineDiff => {
            if let View::Timeline(timeline) = stack.top_mut() {
                timeline.scroll_diff_by(delta);
            }
        }
        ScrollTarget::LogList => {
            if let View::Log(log) = stack.top_mut() {
                log.scroll_by(delta);
            }
        }
        ScrollTarget::InspectorServers => {
            if let View::LspInspector(inspector) = stack.top_mut() {
                inspector.scroll_servers_by(delta);
            }
        }
        ScrollTarget::InspectorDetail => {
            if let View::LspInspector(inspector) = stack.top_mut() {
                inspector.scroll_detail_by(delta);
            }
        }
        ScrollTarget::InspectorJournal => {
            if let View::LspInspector(inspector) = stack.top_mut() {
                inspector.scroll_journal_by(delta);
            }
        }
        // `HoverState::scroll_up`/`scroll_down` take no viewport — they
        // clamp against the rendered line count alone (see that type's
        // docs) — so looping the wheel's fixed row count is all this needs.
        ScrollTarget::HoverPopup => scroll_n(delta, |up| {
            if up {
                hover_state.scroll_up();
            } else {
                hover_state.scroll_down();
            }
        }),
        // Timeline/Log/Servers move *selection* (see their own
        // `scroll_*_by` docs) — `RefsPanel`/`UnitsPanel` are the same
        // shape: neither has an independent scroll offset, only a
        // `selected` index its own render windows around, exactly like
        // `refs_panel::render`'s existing keyboard-driven
        // `select_next`/`select_prev`.
        ScrollTarget::RefsPanel => {
            if let Some(panel) = refs_panel {
                scroll_n(delta, |up| {
                    if up {
                        panel.select_prev();
                    } else {
                        panel.select_next();
                    }
                });
            }
        }
        ScrollTarget::UnitsPanel => {
            if let Some(panel) = units_panel {
                scroll_n(delta, |up| {
                    if up {
                        panel.select_prev();
                    } else {
                        panel.select_next();
                    }
                });
            }
        }
        ScrollTarget::HelpPopup => {
            if let Some(state) = help {
                let filter = state.filter_text().to_owned();
                let total_rows = match help_row_count_cache {
                    Some((cached_filter, count)) if *cached_filter == filter => *count,
                    _ => {
                        let count = help::build_rows(keymap, &filter).len();
                        *help_row_count_cache = Some((filter.clone(), count));
                        count
                    }
                };
                let viewport = help::viewport_rows(area);
                scroll_n(delta, |up| {
                    if up {
                        state.scroll_up();
                    } else {
                        state.scroll_down(total_rows, viewport);
                    }
                });
            }
        }
        // Deliberately consumed and discarded: these overlays have nothing
        // scrollable, and the wheel must not reach content the modal is
        // blocking (see `ScrollTarget`'s docs on these variants). The
        // context menu (issue #23) has no `ScrollTarget` of its own to
        // match here at all — see `FrameGeometry::context_menu_rect`'s
        // docs on why — `ui::mod`'s event loop instead gates the whole
        // wheel arm on `context_menu.is_none()` before ever calling this
        // function.
        ScrollTarget::ScopeMenuModal
        | ScrollTarget::UnitsSetupModal
        | ScrollTarget::ComposeModal => {}
    }
}

/// Calls `step(true)` (for a negative `delta`, i.e. "up") or `step(false)`
/// ("down") `delta.unsigned_abs()` times — the shared "loop the wheel's
/// fixed row count against a pane's existing single-step keyboard methods"
/// shape every selection-based/no-viewport target above uses instead of its
/// own hand-rolled loop. One closure rather than separate `up`/`down`
/// closures: two closures over the same `&mut` target constructed at the
/// same call site can't both borrow it, even though only one of them is
/// ever actually invoked.
fn scroll_n(delta: isize, mut step: impl FnMut(bool)) {
    let up = delta.is_negative();
    for _ in 0..delta.unsigned_abs() {
        step(up);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn hit_on_an_empty_geometry_is_none() {
        let geometry = FrameGeometry::new();
        assert_eq!(geometry.hit(0, 0), None);
    }

    #[test]
    fn hit_outside_every_recorded_rect_is_none() {
        let mut geometry = FrameGeometry::new();
        geometry.record(rect(0, 0, 10, 10), ScrollTarget::DiffPane);
        assert_eq!(geometry.hit(20, 20), None);
    }

    #[test]
    fn hit_inside_a_single_rect_returns_its_target() {
        let mut geometry = FrameGeometry::new();
        geometry.record(rect(0, 0, 10, 10), ScrollTarget::DiffPane);
        assert_eq!(geometry.hit(5, 5), Some(ScrollTarget::DiffPane));
        // The rect's own edges are inside it (`Rect::contains` is
        // inclusive of the top/left, exclusive of width/height past them —
        // pin the boundary this module's callers rely on).
        assert_eq!(geometry.hit(0, 0), Some(ScrollTarget::DiffPane));
        assert_eq!(geometry.hit(9, 9), Some(ScrollTarget::DiffPane));
        assert_eq!(geometry.hit(10, 10), None);
    }

    #[test]
    fn overlapping_rects_the_last_recorded_wins() {
        let mut geometry = FrameGeometry::new();
        geometry.record(rect(0, 0, 20, 20), ScrollTarget::DiffPane);
        // A hover popup drawn on top of part of the diff pane, the way
        // `draw`'s `View::Diff` arm really records them — later in draw
        // order, later in `entries`.
        geometry.record(rect(5, 5, 10, 10), ScrollTarget::HoverPopup);
        assert_eq!(geometry.hit(7, 7), Some(ScrollTarget::HoverPopup));
        // Still the diff pane outside the popup's own rect.
        assert_eq!(geometry.hit(1, 1), Some(ScrollTarget::DiffPane));
    }

    /// [`FrameGeometry::hit`] is a thin wrapper over `hit_rect` (see its own
    /// docs) — pin that `hit_rect` itself carries the same last-recorded-
    /// wins precedence, not just the target `hit` strips down to.
    #[test]
    fn hit_rect_overlap_the_last_recorded_wins_and_carries_its_own_rect() {
        let mut geometry = FrameGeometry::new();
        geometry.record(rect(0, 0, 20, 20), ScrollTarget::DiffPane);
        geometry.record(rect(5, 5, 10, 10), ScrollTarget::HoverPopup);
        assert_eq!(
            geometry.hit_rect(7, 7),
            Some((rect(5, 5, 10, 10), ScrollTarget::HoverPopup))
        );
        assert_eq!(
            geometry.hit_rect(1, 1),
            Some((rect(0, 0, 20, 20), ScrollTarget::DiffPane))
        );
    }

    #[test]
    fn zero_size_rects_are_never_hit() {
        let mut geometry = FrameGeometry::new();
        geometry.record(rect(5, 5, 0, 10), ScrollTarget::DiffPane);
        geometry.record(rect(5, 5, 10, 0), ScrollTarget::DiffFiles);
        assert_eq!(geometry.hit(5, 5), None);
    }

    // ---- files_row_at (issue #21) -----------------------------------------

    /// The outer, border-inclusive rect `draw` records for
    /// `ScrollTarget::DiffFiles` — `pane::inner_rect(30, OUTER)` works out
    /// to `(1, 1, 28, 8)` (one column/row shaved off every side by
    /// `Borders::ALL`), so every test below reasons about that inner rect
    /// by hand rather than re-deriving it.
    const OUTER: Rect = Rect {
        x: 0,
        y: 0,
        width: 30,
        height: 10,
    };

    #[test]
    fn files_row_at_the_top_border_row_is_none() {
        // Row 0 is OUTER's own top border — outside the inner rect
        // (`inner.y == 1`) regardless of scroll offset or row count.
        assert_eq!(files_row_at(OUTER, 0, 100, 5, 0), None);
    }

    #[test]
    fn files_row_at_the_bottom_hint_row_is_none() {
        // Row 9 is OUTER's bottom border, where `sidebar::render`'s hint
        // line draws (`title_bottom`, not an extra row) — still outside the
        // inner rect (`inner.y + inner.height == 9`).
        assert_eq!(files_row_at(OUTER, 0, 100, 5, 9), None);
    }

    #[test]
    fn files_row_at_the_first_inner_row_is_index_zero_at_zero_scroll() {
        assert_eq!(files_row_at(OUTER, 0, 5, 5, 1), Some(0));
    }

    #[test]
    fn files_row_at_a_nonzero_scroll_offset_shifts_every_index() {
        // Inner row 1 (the first content row) is `visible_rows[scroll_offset]`
        // once the tree has been scrolled down 3 rows; inner row 3 is two
        // rows further into that same scrolled view.
        assert_eq!(files_row_at(OUTER, 3, 100, 5, 1), Some(3));
        assert_eq!(files_row_at(OUTER, 3, 100, 5, 3), Some(5));
    }

    #[test]
    fn files_row_at_past_the_visible_row_count_is_none() {
        // Inner row 7 (the last row inside an 8-row-tall inner rect) would
        // resolve to index 6 — past a 5-row tree, i.e. blank space below the
        // final rendered row (req 5).
        assert_eq!(files_row_at(OUTER, 0, 5, 5, 7), None);
        // The row just above it is still in range.
        assert_eq!(files_row_at(OUTER, 0, 5, 5, 2), Some(1));
    }

    #[test]
    fn files_row_at_a_degenerate_outer_rect_never_panics() {
        let zero_width = Rect { width: 0, ..OUTER };
        let zero_height = Rect { height: 0, ..OUTER };
        assert_eq!(files_row_at(zero_width, 0, 100, 0, 0), None);
        assert_eq!(files_row_at(zero_height, 0, 100, 0, 0), None);
        // A click above/left of a normal rect entirely — exercises the same
        // "would underflow if not for the containment guard" path as the
        // degenerate rects above.
        assert_eq!(files_row_at(OUTER, 0, 100, 0, 0), None);
    }

    #[test]
    fn scroll_n_calls_up_for_a_negative_delta_and_down_for_a_positive_one() {
        let mut ups = 0;
        let mut downs = 0;
        scroll_n(-3, |up| if up { ups += 1 } else { downs += 1 });
        assert_eq!((ups, downs), (3, 0));

        let mut ups = 0;
        let mut downs = 0;
        scroll_n(3, |up| if up { ups += 1 } else { downs += 1 });
        assert_eq!((ups, downs), (0, 3));
    }

    // ---- HitRow / resolve_hit (issue #22) ---------------------------------

    /// A representative gutter width — arbitrary but nontrivial, so a test
    /// clicking inside vs. past it exercises a real subtraction rather than
    /// `0` trivially always being "on content."
    const GUTTER: usize = 8;

    #[test]
    fn a_gutter_click_on_a_unified_row_is_position_only() {
        let hit = HitRow::Unified(LineHit {
            row_idx: 4,
            content_start_col: 0,
        });
        let resolved = resolve_hit(hit, GUTTER, 3).unwrap();
        assert_eq!(resolved.row_idx, 4);
        assert!(!resolved.content_click);
        assert!(resolved.new_or_unified_side);
        // Position value is the row's own start column, not a value
        // derived from `local_x` — see `resolve_line_column`'s docs.
        assert_eq!(resolved.display_col, 0);
    }

    #[test]
    fn an_on_content_unified_click_maps_local_x_through_the_gutter_and_wrap_offset() {
        let hit = HitRow::Unified(LineHit {
            row_idx: 4,
            // A continuation row of a wrapped line: 20 columns of the
            // logical line already wrapped away above it.
            content_start_col: 20,
        });
        let resolved = resolve_hit(hit, GUTTER, (GUTTER + 5) as u16).unwrap();
        assert_eq!(resolved.row_idx, 4);
        assert!(resolved.content_click);
        assert!(resolved.new_or_unified_side);
        assert_eq!(resolved.display_col, 25); // 20 + (GUTTER+5 - GUTTER)
    }

    #[test]
    fn a_side_by_side_old_cell_click_is_never_the_new_or_unified_side() {
        let hit = HitRow::SideBySide {
            left_width: 15,
            old: Some(LineHit {
                row_idx: 2,
                content_start_col: 0,
            }),
            new: Some(LineHit {
                row_idx: 2,
                content_start_col: 0,
            }),
        };
        let resolved = resolve_hit(hit, GUTTER, (GUTTER + 3) as u16).unwrap();
        assert_eq!(resolved.row_idx, 2);
        assert!(resolved.content_click);
        assert!(
            !resolved.new_or_unified_side,
            "old cell must never be identifier-eligible, even on a real content click"
        );
    }

    #[test]
    fn a_side_by_side_new_cell_click_shifts_local_x_past_the_left_column_and_divider() {
        let hit = HitRow::SideBySide {
            left_width: 15,
            old: None,
            new: Some(LineHit {
                row_idx: 3,
                content_start_col: 0,
            }),
        };
        // Columns 0..15 are the left cell (`left_width` = 15); column 15
        // itself is the divider (`x == left_width`); the new cell's own
        // local space starts fresh at column 16 (`left_width + 1`) — which
        // is exactly the `16` in the literal below.
        let resolved = resolve_hit(hit, GUTTER, (16 + GUTTER + 2) as u16).unwrap();
        assert_eq!(resolved.row_idx, 3);
        assert!(resolved.content_click);
        assert!(resolved.new_or_unified_side);
        assert_eq!(resolved.display_col, 2);
    }

    #[test]
    fn the_side_by_side_divider_column_itself_is_non_actionable() {
        let hit = HitRow::SideBySide {
            left_width: 15,
            old: Some(LineHit {
                row_idx: 2,
                content_start_col: 0,
            }),
            new: Some(LineHit {
                row_idx: 2,
                content_start_col: 0,
            }),
        };
        assert_eq!(resolve_hit(hit, GUTTER, 15), None);
    }

    #[test]
    fn side_by_side_blank_filler_padding_is_non_actionable() {
        // A `Paired` row where one side wrapped to more visual rows than
        // the other — the shorter side's overflow rows carry `None` (see
        // `HitRow::SideBySide`'s docs).
        let hit = HitRow::SideBySide {
            left_width: 15,
            old: None,
            new: None,
        };
        assert_eq!(resolve_hit(hit, GUTTER, 3), None);
        assert_eq!(resolve_hit(hit, GUTTER, 20), None);
    }

    #[test]
    fn structural_gap_and_comment_body_rows_position_only_never_identifier_eligible() {
        let cases = [
            (HitRow::Structural { flat_idx: 7 }, 7),
            (HitRow::Gap { flat_idx: 9 }, 9),
            (HitRow::CommentBody { anchor_flat_idx: 3 }, 3),
        ];
        for (hit, expected_row) in cases {
            let resolved = resolve_hit(hit, GUTTER, 50).unwrap();
            assert_eq!(resolved.row_idx, expected_row);
            assert!(!resolved.content_click);
            assert!(!resolved.new_or_unified_side);
        }
    }

    #[test]
    fn a_file_line_click_resolves_the_same_way_a_unified_click_does() {
        let hit = HitRow::FileLine(LineHit {
            row_idx: 12,
            content_start_col: 0,
        });
        let resolved = resolve_hit(hit, GUTTER, (GUTTER + 4) as u16).unwrap();
        assert_eq!(resolved.row_idx, 12);
        assert_eq!(resolved.display_col, 4);
        assert!(resolved.content_click);
        assert!(resolved.new_or_unified_side);
    }

    #[test]
    fn diff_row_hit_looks_up_the_row_resolve_hit_needs_by_local_y() {
        let mut geometry = FrameGeometry::new();
        geometry.record_diff_content(
            rect(0, 0, 20, 10),
            vec![
                HitRow::Structural { flat_idx: 0 },
                HitRow::Unified(LineHit {
                    row_idx: 1,
                    content_start_col: 0,
                }),
            ],
        );
        let (_, _, hit) = geometry.diff_row_hit(0, 0).unwrap();
        assert_eq!(resolve_hit(*hit, GUTTER, 0).unwrap().row_idx, 0);
        let (_, _, hit) = geometry.diff_row_hit(0, 1).unwrap();
        assert_eq!(resolve_hit(*hit, GUTTER, 0).unwrap().row_idx, 1);
        // Past the last recorded row — the content rect's own blank
        // remainder — has no `HitRow` to resolve at all.
        assert_eq!(geometry.diff_row_hit(0, 2), None);
    }

    // ---- FrameGeometry::diff_row_hit / file_row_hit (issue #22) -----------

    #[test]
    fn diff_row_hit_is_none_before_any_content_is_recorded() {
        let geometry = FrameGeometry::new();
        assert_eq!(geometry.diff_row_hit(0, 0), None);
        assert_eq!(geometry.file_row_hit(0, 0), None);
    }

    #[test]
    fn diff_row_hit_translates_screen_coordinates_into_the_content_rects_own_space() {
        let mut geometry = FrameGeometry::new();
        let content_rect = rect(5, 2, 20, 10);
        let hits = vec![
            HitRow::Structural { flat_idx: 0 },
            HitRow::Unified(LineHit {
                row_idx: 1,
                content_start_col: 0,
            }),
        ];
        geometry.record_diff_content(content_rect, hits);

        // Screen (5, 3) is the content rect's own (0, 1) — row 1 of `hits`.
        let (local_x, local_y, hit) = geometry.diff_row_hit(5, 3).unwrap();
        assert_eq!((local_x, local_y), (0, 1));
        assert!(matches!(hit, HitRow::Unified(_)));

        // Outside the recorded rect entirely.
        assert_eq!(geometry.diff_row_hit(100, 100), None);
    }

    #[test]
    fn file_row_hit_is_independent_of_diff_content() {
        let mut geometry = FrameGeometry::new();
        geometry.record_diff_content(rect(0, 0, 10, 10), vec![HitRow::Structural { flat_idx: 0 }]);
        // Nothing recorded for the file pane this frame — a diff-only frame
        // (`View::Diff` on top) must never leak into `file_row_hit`.
        assert_eq!(geometry.file_row_hit(0, 0), None);

        geometry.record_file_content(
            rect(0, 0, 10, 10),
            vec![HitRow::FileLine(LineHit {
                row_idx: 0,
                content_start_col: 0,
            })],
        );
        assert!(geometry.file_row_hit(0, 0).is_some());
    }

    // ---- handle_left_click / action resolution (issue #22) ----------------
    //
    // Exercises `handle_left_click`'s `DiffPane` branch end to end against a
    // real `App` and a hand-built `Vec<HitRow>` matching its rows exactly —
    // the same shape `diff_view::render_unified` would really produce, but
    // constructed by hand here so these tests stay independent of the
    // renderer (see this module's own doc comment). `identifier_hit` alone
    // is a *necessary*, not *sufficient*, condition for LSP work: a Del row
    // or a non-interactive `App` can still resolve `identifier_hit: true`
    // (the click really did land on a symbol) while `App::hover_query`
    // — the gate `ui::mod`'s `handle_action` re-derives from the cursor/
    // active-symbol `handle_left_click` just set — reports `None`, so no
    // request ever gets queued. These tests check both halves: the mouse
    // layer's own `identifier_hit` verdict, and the resulting
    // `hover_query()` outcome it would actually gate.

    fn app_with_rows(rows: Vec<crate::diff::DiffRow>) -> crate::ui::app::App {
        let file = crate::diff::DiffFile {
            old_path: Some("a.rs".to_owned()),
            new_path: Some("a.rs".to_owned()),
            hunks: vec![crate::diff::DiffHunk {
                old_start: 1,
                old_lines: rows.len() as u32,
                new_start: 1,
                new_lines: rows.len() as u32,
                header: String::new(),
                known_eof: true,
                rows,
            }],
            ..Default::default()
        };
        crate::ui::app::App::new(
            "test-repo".to_owned(),
            std::path::PathBuf::from("/repo"),
            vec![file],
        )
    }

    fn row(
        kind: crate::diff::DiffLineKind,
        text: &str,
        old: Option<u32>,
        new: Option<u32>,
    ) -> crate::diff::DiffRow {
        crate::diff::DiffRow {
            kind,
            text: text.to_owned(),
            old_line: old,
            new_line: new,
        }
    }

    /// `app`'s rows, flattened the same way `diff_view::render_unified`
    /// would: `Structural`/`Gap` for every non-`Line` row, `Unified` for
    /// every content row (`content_start_col: 0` — none of these test
    /// fixtures wrap).
    fn unified_hits(app: &crate::ui::app::App) -> Vec<HitRow> {
        app.rows
            .iter()
            .enumerate()
            .map(|(idx, r)| match r {
                crate::diff::RenderRow::Line { .. } => HitRow::Unified(LineHit {
                    row_idx: idx,
                    content_start_col: 0,
                }),
                crate::diff::RenderRow::Gap { .. } => HitRow::Gap { flat_idx: idx },
                _ => HitRow::Structural { flat_idx: idx },
            })
            .collect()
    }

    /// A 5-row fixture: file/hunk headers, a `Context` row ("alpha"), a
    /// `Del` row ("removed"), and an `Add` row ("hello world") — enough row
    /// kinds to cover every non-interactive class this issue's acceptance
    /// criteria lists except side-by-side's old-cell suppression (already
    /// pinned above by `a_side_by_side_old_cell_click_is_never_the_new_or_unified_side`,
    /// which needs no real `App` at all) and binary/gap rows (both collapse
    /// to `HitRow::Structural`/`Gap`, already proven position-only by
    /// `structural_gap_and_comment_body_rows_position_only_never_identifier_eligible`).
    fn fixture_app() -> crate::ui::app::App {
        app_with_rows(vec![
            row(
                crate::diff::DiffLineKind::Context,
                "alpha",
                Some(1),
                Some(1),
            ),
            row(crate::diff::DiffLineKind::Del, "removed", Some(2), None),
            row(crate::diff::DiffLineKind::Add, "hello world", None, Some(2)),
        ])
    }

    fn geometry_over(hits: Vec<HitRow>) -> FrameGeometry {
        let mut geometry = FrameGeometry::new();
        geometry.record(rect(0, 0, 60, hits.len() as u16), ScrollTarget::DiffPane);
        geometry.record_diff_content(rect(0, 0, 60, hits.len() as u16), hits);
        geometry
    }

    fn click(
        geometry: &FrameGeometry,
        stack: &mut ViewStack,
        col: u16,
        row: u16,
        shift: bool,
    ) -> bool {
        let mut jump_stack = JumpStack::new();
        let mut hover_state = HoverState::default();
        handle_left_click(
            geometry,
            stack,
            &mut jump_stack,
            &mut hover_state,
            col,
            row,
            shift,
        )
    }

    #[test]
    fn an_add_row_identifier_click_is_eligible_and_a_real_go_to_definition_target() {
        let app = fixture_app();
        let hits = unified_hits(&app);
        let geometry = geometry_over(hits);
        let mut stack = ViewStack::new(View::Diff(app));

        // Row 4 (0=FileHeader,1=HunkHeader,2=Context,3=Del,4=Add): "hello"
        // starts at the gutter's own end (`content_start_col: 0`).
        let hit = click(
            &geometry,
            &mut stack,
            diff_view::gutter_width() as u16,
            4,
            false,
        );
        assert!(hit, "an identifier click on an Add row must be eligible");
        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert_eq!(app.cursor, 4);
        assert!(
            app.hover_query().is_some(),
            "the exact target `handle_action`'s GotoDefinition arm would dispatch on"
        );
    }

    #[test]
    fn a_del_row_identifier_click_matches_a_symbol_but_hover_query_still_refuses_it() {
        let app = fixture_app();
        let hits = unified_hits(&app);
        let geometry = geometry_over(hits);
        let mut stack = ViewStack::new(View::Diff(app));

        // Row 3 is the Del row ("removed") — a real symbol sits at column 0
        // just like the Add row's does, so `identifier_hit` still resolves
        // `true` (the click really did land on a token); `lsp_target`
        // refusing `Del` rows is what actually stops any LSP work, proven
        // via `hover_query` here exactly as `handle_action` would see it.
        let hit = click(
            &geometry,
            &mut stack,
            diff_view::gutter_width() as u16,
            3,
            false,
        );
        assert!(hit);
        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert_eq!(app.cursor, 3);
        assert!(
            app.hover_query().is_none(),
            "a Del row must never resolve an LSP target, even though the click matched a symbol"
        );
    }

    #[test]
    fn a_gutter_click_on_an_eligible_row_only_positions_the_cursor() {
        let app = fixture_app();
        let hits = unified_hits(&app);
        let geometry = geometry_over(hits);
        let mut stack = ViewStack::new(View::Diff(app));

        let hit = click(&geometry, &mut stack, 0, 4, false);
        assert!(!hit, "a gutter click must never be identifier-eligible");
        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert_eq!(
            app.cursor, 4,
            "the row still positions, only the identifier chase is skipped"
        );
    }

    #[test]
    fn a_header_row_click_positions_the_cursor_with_no_identifier_to_resolve() {
        let app = fixture_app();
        let hits = unified_hits(&app);
        let geometry = geometry_over(hits);
        let mut stack = ViewStack::new(View::Diff(app));

        let hit = click(&geometry, &mut stack, 5, 0, false);
        assert!(!hit);
        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn shift_click_on_an_eligible_identifier_positions_but_never_chases_definition() {
        let app = fixture_app();
        let hits = unified_hits(&app);
        let geometry = geometry_over(hits);
        let mut stack = ViewStack::new(View::Diff(app));

        let hit = click(
            &geometry,
            &mut stack,
            diff_view::gutter_width() as u16,
            4,
            true,
        );
        assert!(
            !hit,
            "req 4: shift-click extends selection, never definition, even on a real identifier"
        );
        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert_eq!(
            app.cursor, 4,
            "positioning (and therefore selection extension) still happens"
        );
    }

    #[test]
    fn a_non_interactive_apps_identifier_click_still_matches_but_hover_query_refuses_it() {
        let mut app = fixture_app();
        app.interactive = false;
        let hits = unified_hits(&app);
        let geometry = geometry_over(hits);
        let mut stack = ViewStack::new(View::Diff(app));

        let hit = click(
            &geometry,
            &mut stack,
            diff_view::gutter_width() as u16,
            4,
            false,
        );
        assert!(hit);
        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert!(
            app.hover_query().is_none(),
            "a historical/non-interactive diff must never resolve an LSP target"
        );
    }

    // ---- handle_right_click (issue #23) ------------------------------------

    #[test]
    fn handle_right_click_on_a_diff_row_positions_the_cursor_without_chasing_gd() {
        let app = fixture_app();
        let hits = unified_hits(&app);
        let geometry = geometry_over(hits);
        let mut stack = ViewStack::new(View::Diff(app));
        let mut hover_state = HoverState::default();

        let outcome = handle_right_click(
            &geometry,
            &mut stack,
            &mut hover_state,
            diff_view::gutter_width() as u16,
            4,
        );
        assert_eq!(outcome, RightClickOutcome::Target(MenuTarget::DiffRow));
        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert_eq!(app.cursor, 4, "the click still positions the cursor");
    }

    #[test]
    fn handle_right_click_never_cancels_an_active_visual_selection() {
        let mut app = fixture_app();
        app.cursor = 2; // the Context row — a real selectable start
        assert_eq!(
            app.toggle_visual(),
            crate::ui::app::VisualToggleOutcome::Started
        );
        let hits = unified_hits(&app);
        let geometry = geometry_over(hits);
        let mut stack = ViewStack::new(View::Diff(app));
        let mut hover_state = HoverState::default();

        // Right-click a different, identifier-eligible row — exactly the
        // shape of click that would cancel visual mode via
        // `handle_left_click` (see `handle_diff_pane_click`'s docs).
        handle_right_click(
            &geometry,
            &mut stack,
            &mut hover_state,
            diff_view::gutter_width() as u16,
            4,
        );
        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert!(
            app.visual_active(),
            "resolving a menu target must never discard the selection the \
             menu's own \"Add comment\"/\"Yank selection\" entries need"
        );
    }

    #[test]
    fn handle_right_click_on_the_open_menus_own_rect_is_a_noop() {
        let app = fixture_app();
        let mut geometry = FrameGeometry::new();
        geometry.record_context_menu(rect(0, 0, 20, 5));
        let mut stack = ViewStack::new(View::Diff(app));
        let mut hover_state = HoverState::default();

        assert_eq!(
            handle_right_click(&geometry, &mut stack, &mut hover_state, 5, 2),
            RightClickOutcome::Noop
        );
    }

    /// The bug this pins down: a right-click that misses the menu's own
    /// precise rect, but still lands inside the pane the menu happens to be
    /// drawn over, must resolve against whatever's really there (req 7's
    /// retarget rule) — not be swallowed the way it would be if the popup
    /// rect were instead recorded as a coarse, whole-pane `ScrollTarget`
    /// entry the way the fully-modal overlays are.
    #[test]
    fn handle_right_click_elsewhere_in_the_pane_resolves_past_the_open_menu() {
        let app = fixture_app();
        let hits = unified_hits(&app);
        let mut geometry = geometry_over(hits);
        // The menu's own small popup sits at (0, 0)-(10, 2) — well clear of
        // row 4, where the real `DiffPane`/`diff_content` target is.
        geometry.record_context_menu(rect(0, 0, 10, 2));
        let mut stack = ViewStack::new(View::Diff(app));
        let mut hover_state = HoverState::default();

        let outcome = handle_right_click(
            &geometry,
            &mut stack,
            &mut hover_state,
            diff_view::gutter_width() as u16,
            4,
        );
        assert_eq!(outcome, RightClickOutcome::Target(MenuTarget::DiffRow));
    }

    #[test]
    fn handle_right_click_on_a_fully_modal_overlay_is_a_noop() {
        for target in [
            ScrollTarget::ComposeModal,
            ScrollTarget::ScopeMenuModal,
            ScrollTarget::UnitsSetupModal,
            ScrollTarget::HelpPopup,
        ] {
            let app = fixture_app();
            let mut geometry = FrameGeometry::new();
            geometry.record(rect(0, 0, 20, 5), target);
            let mut stack = ViewStack::new(View::Diff(app));
            let mut hover_state = HoverState::default();

            assert_eq!(
                handle_right_click(&geometry, &mut stack, &mut hover_state, 5, 2),
                RightClickOutcome::Noop,
                "{target:?}"
            );
        }
    }

    #[test]
    fn handle_right_click_on_the_hover_popup_closes_it_and_opens_nothing() {
        let app = fixture_app();
        let mut geometry = FrameGeometry::new();
        geometry.record(rect(0, 0, 20, 5), ScrollTarget::HoverPopup);
        let mut stack = ViewStack::new(View::Diff(app));
        let mut hover_state = HoverState::default();
        hover_state.set_pending();
        assert!(hover_state.status_hint().is_some());

        let outcome = handle_right_click(&geometry, &mut stack, &mut hover_state, 5, 2);
        assert_eq!(outcome, RightClickOutcome::ClosedHover);
        assert!(
            hover_state.status_hint().is_none(),
            "the pending/message state must be cleared, same as `HoverState::close`"
        );
    }

    #[test]
    fn handle_right_click_on_refs_or_units_panel_reports_closed_panel() {
        for target in [ScrollTarget::RefsPanel, ScrollTarget::UnitsPanel] {
            let app = fixture_app();
            let mut geometry = FrameGeometry::new();
            geometry.record(rect(0, 0, 20, 5), target);
            let mut stack = ViewStack::new(View::Diff(app));
            let mut hover_state = HoverState::default();

            assert_eq!(
                handle_right_click(&geometry, &mut stack, &mut hover_state, 5, 2),
                RightClickOutcome::ClosedPanel,
                "{target:?}"
            );
        }
    }

    #[test]
    fn handle_right_click_on_blank_space_is_a_miss() {
        let app = fixture_app();
        let geometry = FrameGeometry::new(); // nothing recorded at all
        let mut stack = ViewStack::new(View::Diff(app));
        let mut hover_state = HoverState::default();

        assert_eq!(
            handle_right_click(&geometry, &mut stack, &mut hover_state, 5, 2),
            RightClickOutcome::Miss
        );
    }

    /// Two right-clicks in a row against different geometry, exercising
    /// both halves of "retarget when valid, close when invalid" (req 7) at
    /// the resolution layer this function owns — `ui::mod`'s open flow is
    /// what actually replaces `context_menu` on each outcome, but both
    /// outcomes it would react to originate here.
    #[test]
    fn handle_right_click_called_twice_resolves_independently_each_time() {
        let app = fixture_app();
        let hits = unified_hits(&app);
        let geometry = geometry_over(hits);
        let mut stack = ViewStack::new(View::Diff(app));
        let mut hover_state = HoverState::default();

        let first = handle_right_click(
            &geometry,
            &mut stack,
            &mut hover_state,
            diff_view::gutter_width() as u16,
            4,
        );
        assert_eq!(first, RightClickOutcome::Target(MenuTarget::DiffRow));

        // A second click on blank space (past the last recorded row) must
        // resolve to `Miss` on its own — not carry over the first click's
        // `Target` outcome.
        let second = handle_right_click(&geometry, &mut stack, &mut hover_state, 5, 99);
        assert_eq!(second, RightClickOutcome::Miss);
    }

    /// A files-tree fixture with one real directory row — `fixture_app`
    /// puts every file at the repo root, too flat to exercise
    /// `MenuTarget::TreeDir` at all.
    fn dir_tree_app() -> crate::ui::app::App {
        let make = |name: &str| crate::diff::DiffFile {
            old_path: Some(name.to_owned()),
            new_path: Some(name.to_owned()),
            hunks: vec![crate::diff::DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                header: String::new(),
                known_eof: true,
                rows: vec![row(
                    crate::diff::DiffLineKind::Context,
                    "content",
                    Some(1),
                    Some(1),
                )],
            }],
            ..Default::default()
        };
        crate::ui::app::App::new(
            "repo".to_owned(),
            std::path::PathBuf::from("/repo"),
            vec![make("dir/a.rs"), make("dir/b.rs")],
        )
    }

    fn geometry_over_files() -> FrameGeometry {
        let mut geometry = FrameGeometry::new();
        geometry.record(OUTER, ScrollTarget::DiffFiles);
        geometry
    }

    #[test]
    fn handle_right_click_on_a_directory_row_selects_without_toggling() {
        let app = dir_tree_app(); // visible_rows[0] == "dir" (a directory)
        assert!(matches!(
            app.visible_rows[0].kind,
            crate::ui::file_tree::VisibleKind::Directory { expanded: true, .. }
        ));
        let geometry = geometry_over_files();
        let mut stack = ViewStack::new(View::Diff(app));
        let mut hover_state = HoverState::default();

        // Inner row 1 (col/row translated through `OUTER`'s border) is
        // `visible_rows[0]`.
        let outcome = handle_right_click(&geometry, &mut stack, &mut hover_state, 5, 1);
        assert_eq!(
            outcome,
            RightClickOutcome::Target(MenuTarget::TreeDir {
                path: "dir".to_owned()
            })
        );
        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert_eq!(app.focus, crate::ui::app::MainPaneFocus::Files);
        assert_eq!(app.files_selection, 0);
        assert!(
            matches!(
                app.visible_rows[0].kind,
                crate::ui::file_tree::VisibleKind::Directory { expanded: true, .. }
            ),
            "a right-click must never toggle the directory it targets"
        );
    }

    #[test]
    fn handle_right_click_on_a_file_row_reports_the_tree_file_target() {
        let app = dir_tree_app(); // visible_rows[1] == "a.rs" (a file)
        let geometry = geometry_over_files();
        let mut stack = ViewStack::new(View::Diff(app));
        let mut hover_state = HoverState::default();

        // Inner row 2 is `visible_rows[1]`.
        let outcome = handle_right_click(&geometry, &mut stack, &mut hover_state, 5, 2);
        assert_eq!(outcome, RightClickOutcome::Target(MenuTarget::TreeFile));
        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert_eq!(app.focus, crate::ui::app::MainPaneFocus::Files);
        assert_eq!(app.files_selection, 1);
        assert_eq!(
            app.cursor, 0,
            "selecting a tree row must never jump the diff cursor"
        );
    }

    #[test]
    fn handle_right_click_past_the_end_of_the_files_tree_is_a_miss() {
        let app = dir_tree_app();
        let geometry = geometry_over_files();
        let mut stack = ViewStack::new(View::Diff(app));
        let mut hover_state = HoverState::default();

        // Inner row 7 is well past the 3-row tree (dir, a.rs, b.rs) —
        // blank space below the last row.
        let outcome = handle_right_click(&geometry, &mut stack, &mut hover_state, 5, 7);
        assert_eq!(outcome, RightClickOutcome::Miss);
    }
}
