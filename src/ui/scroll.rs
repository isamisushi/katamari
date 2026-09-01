//! Vertical-scroll arithmetic shared by every pane that scrolls a flat list
//! of rows by cursor position — the diff view's row list, the file view's
//! line list. Each pane still owns its own `cursor`/`scroll_offset` fields;
//! this module only computes the two numbers every such pane needs: how far
//! a half-page jump moves the cursor, and where the scroll offset must land
//! to keep the cursor on screen.
//!
//! Every function here is generalized over a row's *visual* height via a
//! `row_height: impl Fn(usize) -> usize` callback rather than assuming one
//! logical row always occupies exactly one terminal line — soft-wrapped
//! content (see `ui::text::wrap_spans_to_width`) makes that assumption
//! false whenever a row's text is wider than the pane and `[ui] wrap` is on.
//! A caller with no wrapping to worry about (or `wrap = false`) simply
//! passes `|_| 1`, which makes every function here behave exactly as the
//! uniform-height version these replaced did — see each function's tests
//! for that equivalence pinned down directly.

/// Half of `viewport_height`, floored at 1 so scrolling in a one-row
/// viewport still moves. The *visual*-row budget [`half_page_down`]/
/// [`half_page_up`] travel by.
pub fn half_page(viewport_height: usize) -> usize {
    (viewport_height / 2).max(1)
}

/// `row_height(idx)`, floored at 1 — every row occupies at least one visual
/// row on screen even if a caller's `row_height` somehow reports 0 (an
/// empty logical line still needs a row to sit on).
fn height_of(row_height: &impl Fn(usize) -> usize, idx: usize) -> usize {
    row_height(idx).max(1)
}

/// The scroll offset that keeps `cursor`'s row visible within
/// `viewport_height` *visual* rows, nudging `scroll_offset` only when the
/// cursor's row has moved outside the currently visible window (never
/// re-centering unnecessarily) — the same rule the pre-wrap, uniform-height
/// version of this function followed, generalized via `row_height` to a
/// pane where a row can be more than one visual row tall.
///
/// When the cursor's own row is taller than the whole viewport (an
/// extremely long wrapped line in a short pane), it becomes both the top
/// and bottom visible row — the same graceful truncation
/// `ui::diff_view::render_unified` already applies when a row's content
/// runs past the bottom of the frame.
pub fn clamp_scroll(
    cursor: usize,
    viewport_height: usize,
    scroll_offset: usize,
    row_height: impl Fn(usize) -> usize,
) -> usize {
    if cursor < scroll_offset {
        return cursor;
    }
    let above_cursor: usize = (scroll_offset..cursor)
        .map(|i| height_of(&row_height, i))
        .sum();
    if above_cursor < viewport_height {
        return scroll_offset; // cursor's row already starts within view
    }
    // Pull the offset forward until the cursor's row is the bottom-most
    // visible one: walk backward from `cursor`, including one more
    // preceding row at a time only while it still fits alongside
    // everything already accumulated — unlike a plain "stop once the
    // viewport is full" accumulation, this never overshoots into an offset
    // that would push the cursor's own row back out of view (which a row
    // taller than one visual line makes possible: including it could blow
    // the budget by more than one row's worth in a single step).
    let mut total = height_of(&row_height, cursor);
    let mut offset = cursor;
    while offset > 0 && total + height_of(&row_height, offset - 1) <= viewport_height {
        offset -= 1;
        total += height_of(&row_height, offset);
    }
    offset
}

/// The scroll offset that centers `cursor` within `viewport_height` visual
/// rows — used when a jump (go-to-definition/references, `]d`/`[d`) lands
/// the cursor somewhere off-screen, so the destination appears with context
/// on both sides rather than pinned to whichever edge ordinary movement's
/// [`clamp_scroll`] would leave it at. Walks backward from `cursor`
/// accumulating visual rows until reaching half the viewport, the
/// wrap-aware analogue of the old `cursor.saturating_sub(viewport_height / 2)`.
pub fn center(cursor: usize, viewport_height: usize, row_height: impl Fn(usize) -> usize) -> usize {
    let target = viewport_height / 2;
    let mut travelled = 0usize;
    let mut offset = cursor;
    while travelled < target && offset > 0 {
        offset -= 1;
        travelled += height_of(&row_height, offset);
    }
    offset
}

/// The scroll offset after nudging `current` by `delta` *visual* rows —
/// issue #20's wheel vocabulary, and the one function in this module that
/// moves a pane's viewport without any notion of a cursor at all (contrast
/// [`clamp_scroll`], which always keeps a cursor's row on screen and never
/// takes a signed delta). `delta` is negative for "scroll up," matching
/// [`crossterm::event::MouseEventKind::ScrollUp`]'s sign convention once
/// `ui::mouse::scroll_at` negates its fixed row count.
///
/// The upper bound comes from [`clamp_scroll`] itself: the offset that
/// would land if `last_row` (the final row) were the "cursor" and the
/// starting offset were `0` is exactly the furthest a viewport can usefully
/// scroll — anything past it would leave blank space below the last row,
/// the same "never dangle past the end" guarantee
/// [`crate::ui::help::HelpState::scroll_down`]'s docs describe for the help
/// popup. Reusing `clamp_scroll` here rather than a second max-scroll
/// formula is what keeps the two functions' idea of "the last useful
/// offset" from ever drifting apart.
pub fn scroll_by(
    current: usize,
    delta: isize,
    last_row: usize,
    viewport_height: usize,
    row_height: impl Fn(usize) -> usize,
) -> usize {
    let max = clamp_scroll(last_row, viewport_height, 0, row_height);
    let shifted = current as isize + delta;
    shifted.clamp(0, max as isize) as usize
}

/// The half-open range of logical row indices that are at least partially
/// on screen right now: starting at `scroll_offset`, walking forward while
/// the *visual* rows accumulated so far stay under `viewport_height` — the
/// same accumulation `ui::diff_view::render_unified`'s own draw loop
/// performs (`while lines.len() < viewport_height && idx < rows.len()`),
/// generalized here so a caller that needs "which rows is the reviewer
/// actually looking at" (e.g. `App::mark_visible_reviewed`, scoping a bulk
/// action to what's on screen rather than the whole derived diff) doesn't
/// have to re-derive that loop itself or drag a `Frame` into `App`. A row
/// whose first visual line lands within the viewport is included even if a
/// later wrapped line of it would run past the bottom edge — exactly the
/// partial-row truncation the renderer itself already applies, not a
/// stricter "fully visible" test.
pub fn visible_range(
    scroll_offset: usize,
    viewport_height: usize,
    total_rows: usize,
    row_height: impl Fn(usize) -> usize,
) -> std::ops::Range<usize> {
    let start = scroll_offset.min(total_rows);
    let mut idx = start;
    let mut total = 0usize;
    while idx < total_rows && total < viewport_height {
        total += height_of(&row_height, idx);
        idx += 1;
    }
    start..idx
}

/// `Action::HalfPageDown`'s destination: `cursor` moved forward by
/// [`half_page`]'s *visual*-row budget, landing on whichever logical row
/// that visual distance reaches rather than always advancing by the same
/// number of logical rows — a run of wrapped lines covers fewer logical
/// rows per half-page than a run of ordinary ones. Clamped to `last`.
pub fn half_page_down(
    cursor: usize,
    last: usize,
    viewport_height: usize,
    row_height: impl Fn(usize) -> usize,
) -> usize {
    let target = half_page(viewport_height);
    let mut travelled = 0usize;
    let mut idx = cursor;
    while travelled < target && idx < last {
        idx += 1;
        travelled += height_of(&row_height, idx);
    }
    idx
}

/// As [`half_page_down`], moving backward; clamped to `0`.
pub fn half_page_up(
    cursor: usize,
    viewport_height: usize,
    row_height: impl Fn(usize) -> usize,
) -> usize {
    let target = half_page(viewport_height);
    let mut travelled = 0usize;
    let mut idx = cursor;
    while travelled < target && idx > 0 {
        idx -= 1;
        travelled += height_of(&row_height, idx);
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row is exactly one visual row tall — the pre-wrap behavior
    /// every uniform-height test below pins down still holding once the
    /// same functions are generalized.
    fn uniform(_idx: usize) -> usize {
        1
    }

    #[test]
    fn half_page_is_never_zero_even_in_a_one_row_viewport() {
        assert_eq!(half_page(0), 1);
        assert_eq!(half_page(1), 1);
        assert_eq!(half_page(10), 5);
    }

    // ---- uniform-height equivalence (mirrors the pre-wrap test suite) -----

    #[test]
    fn clamp_scroll_leaves_offset_untouched_when_cursor_already_visible() {
        assert_eq!(clamp_scroll(5, 10, 2, uniform), 2);
    }

    #[test]
    fn clamp_scroll_pulls_offset_down_to_cursor_when_cursor_moved_above_it() {
        assert_eq!(clamp_scroll(1, 10, 5, uniform), 1);
    }

    #[test]
    fn clamp_scroll_pushes_offset_forward_when_cursor_moved_past_viewport_bottom() {
        assert_eq!(clamp_scroll(12, 5, 0, uniform), 8);
    }

    #[test]
    fn center_puts_the_cursor_at_the_viewport_midpoint() {
        assert_eq!(center(20, 10, uniform), 15);
    }

    #[test]
    fn center_clamps_to_zero_near_the_top_of_the_file() {
        assert_eq!(center(2, 10, uniform), 0);
    }

    #[test]
    fn half_page_down_matches_the_old_flat_cursor_plus_half_page() {
        assert_eq!(half_page_down(0, 100, 10, uniform), 5);
    }

    #[test]
    fn half_page_up_matches_the_old_flat_cursor_minus_half_page() {
        assert_eq!(half_page_up(20, 10, uniform), 15);
    }

    #[test]
    fn half_page_down_clamps_to_the_last_row() {
        assert_eq!(half_page_down(96, 100, 10, uniform), 100);
    }

    #[test]
    fn half_page_up_clamps_to_zero() {
        assert_eq!(half_page_up(2, 10, uniform), 0);
    }

    // ---- variable-height (wrapped) behavior --------------------------------

    /// Row 3 is a tall wrapped row (4 visual rows); every other row is
    /// ordinary. Rows: [1,1,1,4,1,1,1,1,1,1].
    fn mixed_heights(idx: usize) -> usize {
        if idx == 3 { 4 } else { 1 }
    }

    #[test]
    fn clamp_scroll_leaves_offset_alone_while_the_cursor_is_still_above_the_tall_row() {
        // Cursor on row 2 (still ordinary height): 2 visual rows precede
        // it, comfortably under a 6-row viewport — no scroll needed even
        // though row 3 just ahead is tall.
        assert_eq!(clamp_scroll(2, 6, 0, mixed_heights), 0);
    }

    #[test]
    fn clamp_scroll_offset_landing_mid_tall_row_still_shows_its_cursor() {
        // From offset 1: row 1 (1) + row 2 (1) precede the cursor on row 3,
        // then row 3's own 4 visual rows — 2 + 4 = 6, exactly filling a
        // 6-row viewport. The cursor's row starts within view (at visual
        // row 2) without needing to scroll further.
        assert_eq!(clamp_scroll(3, 6, 1, mixed_heights), 1);
    }

    #[test]
    fn clamp_scroll_pulls_the_tall_row_and_the_row_after_it_together_when_they_both_fit() {
        // Cursor on row 4 (ordinary height, right after the tall row 3),
        // viewport 5: row 3 (4) + row 4 (1) = 5, exactly filling it — the
        // offset lands on row 3 so both are shown, rather than overshooting
        // to a smaller offset that wouldn't fit or a larger one that would
        // hide row 3 unnecessarily.
        assert_eq!(clamp_scroll(4, 5, 0, mixed_heights), 3);
    }

    #[test]
    fn clamp_scroll_a_row_taller_than_the_viewport_becomes_the_sole_visible_row() {
        // Row 3 alone (height 4) already fills a 4-row viewport with
        // nothing left over for row 4 — the offset must land exactly on
        // the cursor's own row rather than including row 3 as well (which
        // would push the cursor back out of view) or somewhere in between.
        assert_eq!(clamp_scroll(4, 4, 0, mixed_heights), 4);
        // A cursor directly on the tall row itself, in a viewport too
        // short to show all of it, becomes both the top and bottom visible
        // row — the same graceful truncation the renderer applies.
        assert_eq!(clamp_scroll(3, 3, 0, mixed_heights), 3);
    }

    #[test]
    fn center_wrapped_skips_over_a_tall_row_worth_more_visual_distance() {
        // Centering on row 9 with a target of 5 visual rows: walking
        // backward, rows 8,7,6,5 cost 4 (reaching row 5, travelled 4 <
        // target 5), then row 4 costs 1 more (travelled 5, target met) —
        // offset lands on row 4, never needing to cross the tall row 3 at
        // all for this particular cursor position.
        assert_eq!(center(9, 10, mixed_heights), 4);
    }

    #[test]
    fn half_page_down_wrapped_travels_fewer_logical_rows_through_a_tall_one() {
        // From cursor 0, target visual budget is half_page(6) = 3. Rows
        // 1,2 cost 1 each (travelled 2), row 3 costs 4 (travelled 6 >=
        // 3) — half_page_down stops there, having advanced only 3 logical
        // rows to cover a 6-visual-row distance.
        assert_eq!(half_page_down(0, 100, 6, mixed_heights), 3);
    }

    #[test]
    fn half_page_up_wrapped_travels_fewer_logical_rows_through_a_tall_one() {
        // From cursor 9, target visual budget is half_page(6) = 3. Rows
        // 8,7,6 cost 1 each (travelled 3) — met exactly without reaching
        // the tall row 3 at all.
        assert_eq!(half_page_up(9, 6, mixed_heights), 6);
    }

    #[test]
    fn half_page_down_wrapped_clamps_to_the_last_row_even_mid_tall_row() {
        assert_eq!(half_page_down(3, 3, 6, mixed_heights), 3);
    }

    // ---- visible_range --------------------------------------------------

    #[test]
    fn visible_range_covers_exactly_a_full_viewport_of_uniform_rows() {
        assert_eq!(visible_range(0, 5, 100, uniform), 0..5);
        assert_eq!(visible_range(20, 5, 100, uniform), 20..25);
    }

    #[test]
    fn visible_range_stops_early_when_fewer_rows_remain_than_the_viewport() {
        assert_eq!(visible_range(97, 10, 100, uniform), 97..100);
    }

    #[test]
    fn visible_range_is_empty_when_the_offset_is_past_every_row() {
        assert_eq!(visible_range(100, 10, 100, uniform), 100..100);
    }

    #[test]
    fn visible_range_includes_a_row_that_only_starts_within_the_viewport() {
        // Row 3 (index 3) is 4 visual rows tall; a 5-row viewport starting
        // at offset 2 fits rows 2 (1) and 3 (4) exactly, so row 3 is
        // included even though none of the caller's business logic cares
        // that its later wrapped lines would spill past the bottom edge —
        // matching `render_unified`'s own "starts in view" truncation.
        assert_eq!(visible_range(2, 5, 10, mixed_heights), 2..4);
    }

    // ---- scroll_by ----------------------------------------------------

    #[test]
    fn scroll_by_moves_the_offset_by_the_given_delta() {
        assert_eq!(scroll_by(5, 3, 100, 10, uniform), 8);
        assert_eq!(scroll_by(5, -3, 100, 10, uniform), 2);
    }

    #[test]
    fn scroll_by_clamps_at_zero_scrolling_up_past_the_top() {
        assert_eq!(scroll_by(1, -3, 100, 10, uniform), 0);
        assert_eq!(scroll_by(0, -3, 100, 10, uniform), 0);
    }

    #[test]
    fn scroll_by_clamps_at_the_last_useful_offset_scrolling_down_past_the_bottom() {
        // 20 rows, 10-row viewport: the furthest useful offset is 10 (rows
        // 10..=19 fill the viewport exactly) — going past it would leave
        // blank space below row 19.
        assert_eq!(scroll_by(9, 3, 19, 10, uniform), 10);
        assert_eq!(scroll_by(10, 3, 19, 10, uniform), 10);
    }

    #[test]
    fn scroll_by_on_content_that_already_fits_never_leaves_zero() {
        // Fewer rows than the viewport: nothing to scroll to either way.
        assert_eq!(scroll_by(0, 3, 4, 10, uniform), 0);
        assert_eq!(scroll_by(0, -3, 4, 10, uniform), 0);
    }

    #[test]
    fn scroll_by_respects_a_tall_wrapped_row_in_its_upper_bound() {
        // Same fixture as `clamp_scroll`'s own wrapped tests: row 3 is 4
        // visual rows tall. Rows: [1,1,1,4,1,1,1,1,1,1] (10 logical rows),
        // 6-row viewport. The furthest useful offset keeps row 9 (the last)
        // visible without dangling space — same value `clamp_scroll(9, 6,
        // 0, mixed_heights)` would report on its own.
        let max = clamp_scroll(9, 6, 0, mixed_heights);
        assert_eq!(scroll_by(0, 100, 9, 6, mixed_heights), max);
    }

    #[test]
    fn top_and_bottom_are_unaffected_by_row_height_being_plain_cursor_assignment() {
        // `Action::Top`/`Bottom` don't go through this module at all (they
        // assign the cursor directly) — documented here as the reason
        // there's no `top`/`bottom` function to test: nothing about
        // wrapping changes what row index "the first/last row" is.
    }
}
