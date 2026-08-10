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
//! Row-level hit-testing (which logical row a click landed on) is
//! deliberately out of scope here — wheel routing only needs pane
//! *containment*, not a click target. #21/#22 extend this map with that.

use crate::keymap::Keymap;
use crate::ui::help::{self, HelpState};
use crate::ui::hover_popup::HoverState;
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
}

impl FrameGeometry {
    pub fn new() -> Self {
        Self::default()
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

    /// The target whose rect contains `(col, row)`, scanning most- to
    /// least-recently recorded — i.e. reverse draw order. Since draw order
    /// *is* modal precedence (see this module's docs), this is the entire
    /// precedence rule: the last thing drawn on top of a point is what a
    /// wheel event there scrolls, with no separate table of overlay
    /// z-order to keep in sync with `draw`'s own call order.
    pub fn hit(&self, col: u16, row: u16) -> Option<ScrollTarget> {
        let point = Position { x: col, y: row };
        self.entries
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, target)| *target)
    }
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
        // blocking (see `ScrollTarget`'s docs on these variants).
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

    #[test]
    fn zero_size_rects_are_never_hit() {
        let mut geometry = FrameGeometry::new();
        geometry.record(rect(5, 5, 0, 10), ScrollTarget::DiffPane);
        geometry.record(rect(5, 5, 10, 0), ScrollTarget::DiffFiles);
        assert_eq!(geometry.hit(5, 5), None);
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
}
