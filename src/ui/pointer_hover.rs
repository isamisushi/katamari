//! Issue #24: debounced pointer details — resting the mouse on an eligible
//! code symbol or changed-file tree row (rather than clicking it) shows a
//! popup/status note after a short pause, without moving the keyboard
//! cursor, touching `App::active_symbol`, or issuing more than one LSP
//! request per rest.
//!
//! Deliberately a state machine wholly separate from
//! [`crate::ui::hover_popup::HoverState`] rather than a mode bolted onto it:
//! the two must stay independently invalidated (an explicit `Action::Hover`
//! press and a resting pointer can be live at once — see
//! `ui::mod::passive_hover_suppressed`'s precedence rule) and the pointer
//! path additionally tracks tree targets, which have no LSP request at all.
//! What *is* reused wholesale — never re-implemented — is `hover_popup`'s
//! rendering pipeline (`render_hover_contents`, `diagnostics_section`,
//! `popup_rect`, `RenderedHover`, all bumped to `pub(super)` for this
//! module) and the `PartialEq`-is-identity trick `HoverQuery` already gives
//! keyboard hover for free: comparing two `HoverQuery`s is exactly "is this
//! still the same target," which is this module's entire debounce/
//! cancellation vocabulary.
//!
//! `ui::mod`'s event loop owns firing the request and polling its result
//! (see that module's `fire_pointer_hover`/pending-pointer-hover poll) —
//! same "async I/O lives in the event loop, not in a state type" split
//! `hover_popup`'s own module docs describe — this module only ever
//! computes pure state transitions and (for the `Code` popup) pure
//! rendering.

use crate::highlight::LineHighlighter;
use crate::lsp::{HoverResult, LspError};
use crate::ui::app::App;
use crate::ui::diff_view;
use crate::ui::file_tree::NodeId;
use crate::ui::file_view;
use crate::ui::hover_popup::{self, HoverQuery, RenderedHover};
use crate::ui::mouse::{FrameGeometry, ScrollTarget, files_row_at, resolve_hit};
use crate::ui::pane;
use crate::ui::view::{View, ViewStack};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use std::time::{Duration, Instant};

/// How long the pointer must rest on the same target before a request/
/// lookup fires — the issue's recommended figure. Checked once per event-
/// loop iteration against [`crate::ui::EVENT_POLL_INTERVAL`]-paced wall-clock
/// reads (never a `sleep`; see `ui::mod`'s deadline-check call site next to
/// `key_display.tick`), so the true worst-case latency is this plus one
/// poll interval — documented there, not duplicated here.
pub const POINTER_HOVER_DEBOUNCE: Duration = Duration::from_millis(400);

/// What a resting pointer resolved to — the debounce/cancellation identity
/// this whole module keys off of. `Code`'s [`HoverQuery`] already derives
/// `PartialEq` for exactly this purpose (see that type's docs); `Tree`
/// reuses [`NodeId`]'s own identity the same way the changed-files sidebar's
/// selection does.
#[derive(Debug, Clone, PartialEq)]
pub enum PointerTarget {
    Code(HoverQuery),
    Tree(NodeId),
}

/// What a `Shown` pointer popup/note actually displays — `Code` renders as
/// a floating popup (via [`render`]), `Tree` as a status-bar line (via
/// [`PointerHoverState::tree_status_hint`]); never both, never a second
/// tooltip on top of a first (out of scope's "no multiple simultaneous
/// tooltips").
enum PointerContent {
    Code(RenderedHover),
    TreeNote(String),
}

/// The debounce/request lifecycle for one resting-pointer target. Every
/// variant but `Idle` carries the `target` it's about — used by
/// [`PointerHoverState::arm`] to tell "still the same thing" (a no-op; the
/// deadline keeps counting, or the in-flight request keeps waiting) from
/// "something else now" (cancel and restart), across every stage from first
/// arming through a shown result — not just while the debounce timer is
/// still running.
#[derive(Default)]
enum PointerStatus {
    #[default]
    Idle,
    /// Debounce running; `deadline` is when [`PointerHoverState::due`]
    /// starts returning this target.
    Armed {
        target: PointerTarget,
        anchor_row: u16,
        deadline: Instant,
    },
    /// A `Code` request is in flight — never reached for `Tree` (a tree
    /// lookup is synchronous, see `ui::mod::fire_pointer_hover`, so it goes
    /// straight from `Armed` to `Shown`). `diagnostics_prefix` is computed
    /// synchronously at dispatch time (mirroring
    /// [`hover_popup::HoverState::set_diagnostics_prefix`]'s identical
    /// reasoning) and carried here until the response lands, since the
    /// request itself only ever answers "what does the server say," not
    /// "what do already-known diagnostics say."
    Pending {
        target: PointerTarget,
        anchor_row: u16,
        diagnostics_prefix: Vec<Line<'static>>,
    },
    Shown {
        target: PointerTarget,
        anchor_row: u16,
        content: PointerContent,
    },
}

/// Issue #24's whole state machine: which target (if any) the pointer is
/// resting on, and how far along showing details for it has gotten. `Idle`
/// by default — the same "nothing until something happens" starting point
/// [`hover_popup::HoverState`] uses. `generation` is this module's own
/// counter, deliberately never shared with `HoverState`'s — an explicit
/// hover and a passive one can be in flight independently (see
/// `ui::mod::passive_hover_suppressed`), and conflating their staleness
/// counters would let one's cancellation silently invalidate the other's
/// perfectly-live request.
#[derive(Default)]
pub struct PointerHoverState {
    generation: u64,
    status: PointerStatus,
}

impl PointerHoverState {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The target every non-`Idle` status carries — `arm`'s "is this the
    /// same thing already in progress" check, factored out since three of
    /// the four variants share the exact same field to read.
    fn current_target(&self) -> Option<&PointerTarget> {
        match &self.status {
            PointerStatus::Idle => None,
            PointerStatus::Armed { target, .. }
            | PointerStatus::Pending { target, .. }
            | PointerStatus::Shown { target, .. } => Some(target),
        }
    }

    /// Arms (or re-arms) the debounce for `target`, resolved at `anchor_row`
    /// as of `now`. A no-op — the deadline (or in-flight request, or shown
    /// popup) keeps counting/standing exactly as it was — when `target`
    /// already matches whatever's in progress, which is what lets a mouse
    /// jittering within one wide symbol's column span, or a tooltip already
    /// on screen, survive repeated `Moved` events without restarting
    /// anything (req 5 only asks for *another* target, or a real
    /// cancellation trigger, to reset the debounce). Otherwise cancels
    /// whatever was in progress (bumping `generation`, so a still-in-flight
    /// response for the old target is dropped as stale on arrival — see
    /// `ui::mod`'s pending-pointer-hover poll) and starts a fresh `Armed`
    /// countdown for the new one.
    pub fn arm(&mut self, target: PointerTarget, anchor_row: u16, now: Instant) {
        if self.current_target() == Some(&target) {
            return;
        }
        self.generation += 1;
        self.status = PointerStatus::Armed {
            target,
            anchor_row,
            deadline: now + POINTER_HOVER_DEBOUNCE,
        };
    }

    /// Cancels whatever's in progress unconditionally — the shared body of
    /// every one of req 5's cancellation triggers (a key press, a non-`Moved`
    /// mouse event, a resize, a watch refresh) and of a not-ready/suppressed
    /// deadline fire. Always drops to `Idle` and bumps `generation`, so an
    /// in-flight `Code` response that lands afterward is discarded as stale
    /// rather than reviving a popup for a target the reviewer has already
    /// moved past.
    pub fn cancel(&mut self) {
        self.generation += 1;
        self.status = PointerStatus::Idle;
    }

    /// `Some((target, anchor_row))`, cloned out, once an `Armed` debounce's
    /// deadline has passed as of `now` — `ui::mod`'s wall-clock preamble
    /// checks this once per loop iteration (see [`POINTER_HOVER_DEBOUNCE`]'s
    /// docs on the resulting worst-case latency) and, if `Some`, dispatches
    /// through `fire_pointer_hover`. Returns owned data (not a reference)
    /// since the caller immediately needs `&mut self` again to record the
    /// dispatch (`set_pending`/`show`) — a borrow tied to `&self` would make
    /// that impossible without an intermediate clone at the call site
    /// anyway. Never mutates `self`: firing (or declining to, e.g. because
    /// readiness isn't `Ready`) is the caller's decision, made with
    /// information (`LspManager`, suppression state) this module doesn't
    /// have.
    pub fn due(&self, now: Instant) -> Option<(PointerTarget, u16)> {
        match &self.status {
            PointerStatus::Armed {
                target,
                anchor_row,
                deadline,
            } if *deadline <= now => Some((target.clone(), *anchor_row)),
            _ => None,
        }
    }

    /// Records a `Code` request as in flight — `ui::mod::fire_pointer_hover`
    /// calls this immediately after submitting it, storing the
    /// synchronously-known diagnostics prefix alongside so [`Self::apply`]
    /// has it once the response lands. Deliberately does not bump
    /// `generation` — mirrors [`hover_popup::HoverState::set_pending`], which
    /// doesn't either: arming already bumped it for this target, and a
    /// pending request is still describing the *same* target, not a new one.
    pub fn set_pending(
        &mut self,
        target: PointerTarget,
        anchor_row: u16,
        diagnostics_prefix: Vec<Line<'static>>,
    ) {
        self.status = PointerStatus::Pending {
            target,
            anchor_row,
            diagnostics_prefix,
        };
    }

    /// Shows `content` for `target` directly — the `Tree` path's whole
    /// dispatch (a synchronous local lookup has nothing to be `Pending`
    /// about) and [`Self::apply`]'s successful-`Code`-response outcome.
    fn show(&mut self, target: PointerTarget, anchor_row: u16, content: PointerContent) {
        self.status = PointerStatus::Shown {
            target,
            anchor_row,
            content,
        };
    }

    /// As [`Self::show`], for the `Tree` case specifically — `ui::mod`'s
    /// only way to reach a `Tree` popup, since `PointerContent` itself stays
    /// private to this module (nothing outside it needs to construct a
    /// `Code` variant, which only ever comes from [`Self::apply`]'s own
    /// rendering).
    pub fn show_tree_note(&mut self, target: NodeId, anchor_row: u16, text: String) {
        self.show(
            PointerTarget::Tree(target),
            anchor_row,
            PointerContent::TreeNote(text),
        );
    }

    /// Applies a `Code` hover response tagged with `generation` — the
    /// passive analogue of [`hover_popup::HoverState::apply`], with one
    /// deliberate divergence (issue #24 plan §4, an accepted judgment call):
    /// `Ok(None)`/`Err` drop silently back to `Idle` rather than surfacing a
    /// status-bar message the way an explicit hover's `Message` state does
    /// — there's no press for a "nothing here"/error note to answer, and a
    /// passive popup that sometimes pops up a *complaint* instead of just
    /// staying quiet would be worse than the debounce it just waited
    /// through. `ui::mod` still records every outcome (including these) to
    /// the observability journal — see its own poll site — only the
    /// *visible* state stays silent.
    ///
    /// A stale `generation` (the target changed, or was cancelled, since
    /// this request was issued) leaves `self` untouched entirely, on
    /// purpose: `self.status` may already describe something newer (a fresh
    /// `Armed`/`Pending`/`Shown` for a different target), and blindly
    /// resetting to `Idle` here would clobber it.
    pub fn apply(
        &mut self,
        generation: u64,
        result: Result<HoverResult, LspError>,
        highlighter: &mut LineHighlighter,
    ) {
        if generation != self.generation {
            return;
        }
        let PointerStatus::Pending {
            target,
            anchor_row,
            diagnostics_prefix,
        } = std::mem::replace(&mut self.status, PointerStatus::Idle)
        else {
            // Unreachable in practice: `generation` only ever matches
            // `self.generation` while a `Pending` request issued under that
            // same generation is still the live status — `arm`/`cancel` are
            // the only other things that touch `generation`, and both bump
            // it. Defensive rather than a `panic!`/`unreachable!`, the same
            // call this codebase's other "shouldn't happen" branches make.
            return;
        };
        let Ok(response) = result else {
            return; // silent Idle — see this method's docs
        };
        let Some(hover) = response else {
            return; // silent Idle — see this method's docs
        };
        let mut lines = diagnostics_prefix;
        let has_diagnostics = !lines.is_empty();
        if has_diagnostics {
            lines.push(Line::default());
        }
        lines.extend(hover_popup::render_hover_contents(
            &hover.contents,
            highlighter,
        ));
        self.status = PointerStatus::Shown {
            target,
            anchor_row,
            content: PointerContent::Code(RenderedHover { lines, scroll: 0 }),
        };
    }

    /// A status-bar note for a `Shown` `Tree` target — issue #24 req 9's
    /// "compact tooltip or status note that does not obscure the pointed
    /// row," slotted into `ui::mod`'s `status_note` chain right after
    /// `hover_state.status_hint()` (see that call site). `None` for every
    /// other status, including a `Shown` `Code` popup — that one speaks for
    /// itself on screen via [`render`], the same way an open keyboard-hover
    /// popup needs no separate status-bar echo.
    pub fn tree_status_hint(&self) -> Option<String> {
        match &self.status {
            PointerStatus::Shown {
                content: PointerContent::TreeNote(text),
                ..
            } => Some(text.clone()),
            _ => None,
        }
    }
}

/// Renders a `Shown` `Code` popup at its own `anchor_row` within `area` —
/// `ui::mod::draw`'s `else` half of "explicit hover wins, otherwise the
/// pointer's" (see that call site). Deliberately takes no
/// `&mut FrameGeometry`: unlike [`hover_popup::render`], this never calls
/// `geometry.record` — issue #24's passive popup isn't a wheel/click target
/// (plan risk 3), so there is nothing here for a `ScrollTarget` to name. A
/// `Shown` `Tree` note, or any other status, draws nothing — it renders as a
/// status-bar line instead (see [`PointerHoverState::tree_status_hint`]).
pub fn render(frame: &mut Frame, area: Rect, state: &PointerHoverState) {
    if let PointerStatus::Shown {
        anchor_row,
        content: PointerContent::Code(rendered),
        ..
    } = &state.status
    {
        let rect = hover_popup::popup_rect(area, *anchor_row);
        hover_popup::render_popup_frame(frame, rect, rendered);
    }
}

/// Resolves `(col, row)` against this frame's recorded geometry into a
/// passive-hover target, mirroring `mouse::handle_left_click`'s own
/// pane-dispatch shape but read-only throughout: no cursor move, no
/// `active_symbol` change, no `App`/`FileView` mutation of any kind (req 3).
/// `anchor_row` is the same content-relative row
/// [`crate::ui::view::View::cursor_screen_row`] already uses for the
/// keyboard-hover popup's own anchor (`local_y` from `diff_row_hit`/
/// `file_row_hit` shares that exact coordinate space, being drawn by the
/// same render loop) — [`render`] hands it straight to
/// [`hover_popup::popup_rect`] unchanged.
///
/// `DiffPane`/`FilePane` require a `content_click` hit on the new/unified
/// side (a gutter, a side-by-side old cell, a structural/gap/comment-body
/// row all resolve to `None` here — the same eligibility
/// `mouse::handle_diff_pane_click`'s identifier-hit check applies, reused
/// rather than re-derived) and then hand off to `hover_query_at`, which
/// requires an *actual* symbol match (no click-path symbol-`0` fallback —
/// see that method's docs: a pointer rest on whitespace must resolve to
/// nothing, not a stale/wrong identifier). `DiffFiles` resolves to a `Tree`
/// target via the same `files_row_at` row lookup `mouse::handle_files_click`
/// uses. Every other recorded target — overlays, the timeline/log/inspector
/// panes, blank space, a miss — is `None`: this module owns exactly the two
/// surfaces req 1/8 name, nothing else.
pub fn resolve_target(
    geometry: &FrameGeometry,
    stack: &ViewStack,
    col: u16,
    row: u16,
) -> Option<(PointerTarget, u16)> {
    match geometry.hit_rect(col, row)? {
        (_, ScrollTarget::DiffPane) => {
            let (local_x, local_y, hit) = geometry.diff_row_hit(col, row)?;
            let resolved = resolve_hit(*hit, diff_view::gutter_width(), local_x)?;
            if !resolved.content_click || !resolved.new_or_unified_side {
                return None;
            }
            // `stack.top()`, not a `let View::Diff(app) = ...` match: this
            // frame's `diff_content` was only ever recorded from `draw`'s
            // own `View::Diff` arm (see `FrameGeometry::diff_content`'s
            // docs), so `View::hover_query_at`'s dispatch already resolves
            // to `App::hover_query_at` here — going through it rather than
            // re-matching the view type is what makes `hover_query_at`
            // exist on `View` at all (mirroring `hover_query`/
            // `cursor_screen_row`'s own per-view dispatch just above it).
            let query = stack
                .top()
                .hover_query_at(resolved.row_idx, resolved.display_col)?;
            Some((PointerTarget::Code(query), local_y))
        }
        (_, ScrollTarget::FilePane) => {
            let (local_x, local_y, hit) = geometry.file_row_hit(col, row)?;
            let resolved = resolve_hit(*hit, file_view::gutter_width(), local_x)?;
            if !resolved.content_click || !resolved.new_or_unified_side {
                return None;
            }
            let query = stack
                .top()
                .hover_query_at(resolved.row_idx, resolved.display_col)?;
            Some((PointerTarget::Code(query), local_y))
        }
        (rect, ScrollTarget::DiffFiles) => {
            let View::Diff(app) = stack.top() else {
                return None;
            };
            let idx = files_row_at(
                rect,
                app.files_scroll_offset,
                app.visible_rows.len(),
                col,
                row,
            )?;
            let inner = pane::inner_rect(rect.width, rect);
            let anchor_row = row.saturating_sub(inner.y);
            Some((
                PointerTarget::Tree(app.visible_rows[idx].id.clone()),
                anchor_row,
            ))
        }
        _ => None,
    }
}

/// The `Tree` half of `ui::mod::fire_pointer_hover`'s dispatch — looks
/// `target` back up in `app.visible_rows` (it may have scrolled out of the
/// tree, or the tree may have rebuilt, between arming and the debounce
/// firing) and formats its tooltip via
/// [`crate::ui::file_tree::tooltip_line`]. Kept here rather than inlined at
/// the one call site so the "row vanished" `None` fallback — silently
/// cancelling instead of showing a note about nothing — is documented once,
/// next to the lookup itself.
pub fn tree_note_for(app: &App, target: &NodeId) -> Option<String> {
    let row = app.visible_rows.iter().find(|row| &row.id == target)?;
    Some(crate::ui::file_tree::tooltip_line(row, &app.files))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffFile, DiffHunk, DiffLineKind, DiffRow};
    use crate::ui::mouse::HitRow;

    fn query(display_col: usize) -> HoverQuery {
        HoverQuery {
            file: "a.rs".into(),
            git_root: "/repo".into(),
            line: 0,
            line_text: "hello world".to_owned(),
            display_col,
        }
    }

    fn t0() -> Instant {
        Instant::now()
    }

    // ---- arm / due ----------------------------------------------------

    #[test]
    fn a_fresh_arm_is_not_due_before_the_debounce_elapses() {
        let mut state = PointerHoverState::default();
        let now = t0();
        state.arm(PointerTarget::Code(query(0)), 3, now);
        assert_eq!(
            state.due(now + POINTER_HOVER_DEBOUNCE - Duration::from_millis(1)),
            None,
            "399ms of a 400ms debounce must not be due yet"
        );
    }

    #[test]
    fn due_fires_exactly_at_the_debounce_deadline() {
        let mut state = PointerHoverState::default();
        let now = t0();
        let target = PointerTarget::Code(query(0));
        state.arm(target.clone(), 3, now);
        assert_eq!(state.due(now + POINTER_HOVER_DEBOUNCE), Some((target, 3)));
    }

    #[test]
    fn arming_the_same_target_again_is_a_no_op_the_deadline_keeps_counting() {
        let mut state = PointerHoverState::default();
        let now = t0();
        let target = PointerTarget::Code(query(0));
        state.arm(target.clone(), 3, now);
        let generation = state.generation();
        // Re-armed 100ms later at a slightly different anchor row (as a
        // wrapped continuation row might report) but the *same* target.
        state.arm(target.clone(), 4, now + Duration::from_millis(100));
        assert_eq!(
            state.generation(),
            generation,
            "no reset for the same target"
        );
        // Still due at the *original* deadline, not a fresh one 100ms later.
        assert_eq!(state.due(now + POINTER_HOVER_DEBOUNCE), Some((target, 3)));
    }

    #[test]
    fn arming_a_different_target_resets_the_deadline_and_bumps_generation() {
        let mut state = PointerHoverState::default();
        let now = t0();
        state.arm(PointerTarget::Code(query(0)), 3, now);
        let generation = state.generation();
        let next = PointerTarget::Code(query(6));
        state.arm(next.clone(), 5, now + Duration::from_millis(100));
        assert_ne!(
            state.generation(),
            generation,
            "a new target bumps generation"
        );
        assert_eq!(
            state.due(now + POINTER_HOVER_DEBOUNCE),
            None,
            "old deadline no longer applies"
        );
        assert_eq!(
            state.due(now + Duration::from_millis(100) + POINTER_HOVER_DEBOUNCE),
            Some((next, 5))
        );
    }

    #[test]
    fn arming_while_pending_the_same_target_is_still_a_no_op() {
        let mut state = PointerHoverState::default();
        let now = t0();
        let target = PointerTarget::Code(query(0));
        state.arm(target.clone(), 3, now);
        state.set_pending(target.clone(), 3, Vec::new());
        let generation = state.generation();
        state.arm(target, 3, now + Duration::from_millis(50));
        assert_eq!(state.generation(), generation);
        assert!(matches!(state.status, PointerStatus::Pending { .. }));
    }

    #[test]
    fn arming_while_shown_a_different_target_cancels_the_shown_popup() {
        let mut state = PointerHoverState::default();
        let now = t0();
        let tree_id = NodeId {
            path: "src".to_owned(),
            is_directory: true,
        };
        state.show_tree_note(tree_id, 0, "src (1 changed)".to_owned());
        assert!(matches!(state.status, PointerStatus::Shown { .. }));
        state.arm(PointerTarget::Code(query(0)), 3, now);
        assert!(matches!(state.status, PointerStatus::Armed { .. }));
    }

    // ---- cancel ---------------------------------------------------------

    #[test]
    fn cancel_from_idle_stays_idle_and_still_bumps_generation() {
        let mut state = PointerHoverState::default();
        let generation = state.generation();
        state.cancel();
        assert!(matches!(state.status, PointerStatus::Idle));
        assert_ne!(state.generation(), generation);
    }

    #[test]
    fn cancel_from_armed_drops_to_idle() {
        let mut state = PointerHoverState::default();
        state.arm(PointerTarget::Code(query(0)), 3, t0());
        state.cancel();
        assert!(matches!(state.status, PointerStatus::Idle));
    }

    #[test]
    fn cancel_from_pending_drops_to_idle() {
        let mut state = PointerHoverState::default();
        let target = PointerTarget::Code(query(0));
        state.arm(target.clone(), 3, t0());
        state.set_pending(target, 3, Vec::new());
        state.cancel();
        assert!(matches!(state.status, PointerStatus::Idle));
    }

    #[test]
    fn cancel_from_shown_drops_to_idle() {
        let mut state = PointerHoverState::default();
        state.show_tree_note(NodeIdFixture::dir("src"), 0, "src (1 changed)".to_owned());
        state.cancel();
        assert!(matches!(state.status, PointerStatus::Idle));
    }

    /// A tiny local alias so the `Shown`/`cancel` tests above don't have to
    /// repeat `crate::ui::file_tree::NodeId { path: ..., is_directory: ... }`
    /// at every call site.
    struct NodeIdFixture;
    impl NodeIdFixture {
        fn dir(path: &str) -> NodeId {
            NodeId {
                path: path.to_owned(),
                is_directory: true,
            }
        }
    }

    // ---- set_pending / apply --------------------------------------------

    #[test]
    fn set_pending_does_not_bump_generation() {
        let mut state = PointerHoverState::default();
        let target = PointerTarget::Code(query(0));
        state.arm(target.clone(), 3, t0());
        let generation = state.generation();
        state.set_pending(target, 3, Vec::new());
        assert_eq!(state.generation(), generation);
    }

    #[test]
    fn apply_for_a_stale_generation_is_dropped_and_leaves_state_untouched() {
        let mut state = PointerHoverState::default();
        let target = PointerTarget::Code(query(0));
        state.arm(target.clone(), 3, t0());
        state.set_pending(target, 3, Vec::new());
        let stale_generation = state.generation();
        // The target changes before the response arrives.
        state.arm(PointerTarget::Code(query(6)), 5, t0());
        let mut hl = LineHighlighter::new();
        state.apply(stale_generation, Ok(None), &mut hl);
        // Still describing the *new* target, untouched by the stale apply.
        assert!(matches!(
            &state.status,
            PointerStatus::Armed { target: PointerTarget::Code(q), .. } if q.display_col == 6
        ));
    }

    #[test]
    fn apply_ok_none_silently_returns_to_idle() {
        let mut state = PointerHoverState::default();
        let target = PointerTarget::Code(query(0));
        state.arm(target.clone(), 3, t0());
        state.set_pending(target, 3, Vec::new());
        let generation = state.generation();
        let mut hl = LineHighlighter::new();
        state.apply(generation, Ok(None), &mut hl);
        assert!(matches!(state.status, PointerStatus::Idle));
    }

    #[test]
    fn apply_err_silently_returns_to_idle() {
        let mut state = PointerHoverState::default();
        let target = PointerTarget::Code(query(0));
        state.arm(target.clone(), 3, t0());
        state.set_pending(target, 3, Vec::new());
        let generation = state.generation();
        let mut hl = LineHighlighter::new();
        state.apply(generation, Err(LspError::Closed), &mut hl);
        assert!(matches!(state.status, PointerStatus::Idle));
    }

    #[test]
    fn apply_ok_some_shows_the_rendered_content() {
        let mut state = PointerHoverState::default();
        let target = PointerTarget::Code(query(0));
        state.arm(target.clone(), 3, t0());
        state.set_pending(target, 3, Vec::new());
        let generation = state.generation();
        let mut hl = LineHighlighter::new();
        let hover = lsp_types::Hover {
            contents: lsp_types::HoverContents::Scalar(lsp_types::MarkedString::String(
                "info".to_owned(),
            )),
            range: None,
        };
        state.apply(generation, Ok(Some(hover)), &mut hl);
        assert!(matches!(
            state.status,
            PointerStatus::Shown {
                content: PointerContent::Code(_),
                ..
            }
        ));
    }

    // ---- structural one-request supersession -----------------------------

    #[test]
    fn a_second_arm_before_the_first_fires_means_only_one_request_can_ever_be_pending() {
        // `set_pending` is the only way `pending_pointer_hover` in `ui::mod`
        // gets populated, and it's only ever called once per `Armed`
        // deadline fire — arming a *different* target before that happens
        // cancels the `Armed` outright (never reaches `Pending`), which is
        // what structurally caps this at one in-flight request. Pinned here
        // as a state-machine fact: two arms in a row for different targets
        // always leave exactly one target (the last) armed or pending, never
        // two.
        let mut state = PointerHoverState::default();
        state.arm(PointerTarget::Code(query(0)), 3, t0());
        state.arm(PointerTarget::Code(query(6)), 3, t0());
        state.arm(PointerTarget::Code(query(11)), 3, t0());
        match &state.status {
            PointerStatus::Armed {
                target: PointerTarget::Code(q),
                ..
            } => assert_eq!(q.display_col, 11, "only the last-armed target survives"),
            PointerStatus::Armed { .. } => panic!("expected a Code target"),
            _ => panic!("expected Armed"),
        }
    }

    // ---- HoverQuery identity ----------------------------------------------

    #[test]
    fn hover_query_for_an_adjacent_symbol_is_a_different_target() {
        // "hello world": "hello" starts at 0, "world" at 6 — two distinct
        // symbols, so their queries (and therefore their `PointerTarget`s)
        // must never compare equal.
        assert_ne!(query(0), query(6));
    }

    #[test]
    fn hover_query_for_the_same_symbol_span_is_the_same_target_regardless_of_jitter() {
        // Two `Moved` events landing at different columns *within* the same
        // symbol's span both resolve (via `App::hover_query_at`) to the same
        // `display_col` (the symbol's own start) — this just pins that a
        // `HoverQuery` built from that shared `display_col` compares equal,
        // which is what makes `arm`'s same-target no-op fire for pointer
        // jitter within one symbol.
        assert_eq!(query(0), query(0));
    }

    // ---- resolve_target ---------------------------------------------------

    fn app_with_rows(rows: Vec<DiffRow>) -> App {
        let file = DiffFile {
            old_path: Some("a.rs".to_owned()),
            new_path: Some("a.rs".to_owned()),
            hunks: vec![DiffHunk {
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
        App::new(
            "test-repo".to_owned(),
            std::path::PathBuf::from("/repo"),
            vec![file],
        )
    }

    fn row(kind: DiffLineKind, text: &str, old: Option<u32>, new: Option<u32>) -> DiffRow {
        DiffRow {
            kind,
            text: text.to_owned(),
            old_line: old,
            new_line: new,
        }
    }

    fn fixture_app() -> App {
        app_with_rows(vec![
            row(DiffLineKind::Context, "alpha", Some(1), Some(1)),
            row(DiffLineKind::Del, "removed", Some(2), None),
            row(DiffLineKind::Add, "hello world", None, Some(2)),
        ])
    }

    fn unified_hits(app: &App) -> Vec<HitRow> {
        app.rows
            .iter()
            .enumerate()
            .map(|(idx, r)| match r {
                crate::diff::RenderRow::Line { .. } => HitRow::Unified(crate::ui::mouse::LineHit {
                    row_idx: idx,
                    content_start_col: 0,
                }),
                crate::diff::RenderRow::Gap { .. } => HitRow::Gap { flat_idx: idx },
                _ => HitRow::Structural { flat_idx: idx },
            })
            .collect()
    }

    fn geometry_over(hits: Vec<HitRow>) -> FrameGeometry {
        let mut geometry = FrameGeometry::new();
        geometry.record(rect(0, 0, 60, hits.len() as u16), ScrollTarget::DiffPane);
        geometry.record_diff_content(rect(0, 0, 60, hits.len() as u16), hits);
        geometry
    }

    fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn resolve_target_on_an_add_row_identifier_resolves_a_code_target() {
        let app = fixture_app();
        let hits = unified_hits(&app);
        let geometry = geometry_over(hits);
        let stack = ViewStack::new(View::Diff(app));

        let resolved = resolve_target(&geometry, &stack, diff_view::gutter_width() as u16, 4);
        assert!(matches!(resolved, Some((PointerTarget::Code(_), 4))));
    }

    #[test]
    fn resolve_target_on_a_del_row_is_none() {
        let app = fixture_app();
        let hits = unified_hits(&app);
        let geometry = geometry_over(hits);
        let stack = ViewStack::new(View::Diff(app));

        // Row 3 (0=FileHeader,1=HunkHeader,2=Context,3=Del): "removed".
        let resolved = resolve_target(&geometry, &stack, diff_view::gutter_width() as u16, 3);
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_target_on_the_gutter_is_none() {
        let app = fixture_app();
        let hits = unified_hits(&app);
        let geometry = geometry_over(hits);
        let stack = ViewStack::new(View::Diff(app));

        let resolved = resolve_target(&geometry, &stack, 0, 4);
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_target_on_a_header_row_is_none() {
        let app = fixture_app();
        let hits = unified_hits(&app);
        let geometry = geometry_over(hits);
        let stack = ViewStack::new(View::Diff(app));

        let resolved = resolve_target(&geometry, &stack, diff_view::gutter_width() as u16, 0);
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_target_outside_any_recorded_rect_is_none() {
        let app = fixture_app();
        let hits = unified_hits(&app);
        let geometry = geometry_over(hits);
        let stack = ViewStack::new(View::Diff(app));

        let resolved = resolve_target(&geometry, &stack, 200, 200);
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_target_in_the_files_tree_resolves_a_tree_target() {
        let file = DiffFile {
            old_path: Some("src/a.rs".to_owned()),
            new_path: Some("src/a.rs".to_owned()),
            ..Default::default()
        };
        let app = App::new(
            "test-repo".to_owned(),
            std::path::PathBuf::from("/repo"),
            vec![file],
        );
        let mut geometry = FrameGeometry::new();
        // Sidebar outer rect: a 1-row border top, so row 1 is the first
        // selectable tree row (mirrors `pane::inner_rect`'s top border).
        geometry.record(rect(0, 0, 20, 5), ScrollTarget::DiffFiles);
        let stack = ViewStack::new(View::Diff(app));

        let resolved = resolve_target(&geometry, &stack, 2, 1);
        assert!(matches!(resolved, Some((PointerTarget::Tree(_), _))));
    }
}
