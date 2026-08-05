//! Vertical-scroll arithmetic shared by every pane that scrolls a flat list
//! of rows by cursor position — the diff view's row list, the file view's
//! line list. Each pane still owns its own `cursor`/`scroll_offset` fields;
//! this module only computes the two numbers every such pane needs: how far
//! a half-page jump moves the cursor, and where the scroll offset must land
//! to keep the cursor on screen.

/// Half of `viewport_height`, floored at 1 so scrolling in a one-row
/// viewport still moves.
pub fn half_page(viewport_height: usize) -> usize {
    (viewport_height / 2).max(1)
}

/// The scroll offset that keeps `cursor` visible within `viewport_height`
/// rows, nudging `scroll_offset` only when the cursor has moved outside the
/// currently visible window (never re-centering unnecessarily).
pub fn clamp_scroll(cursor: usize, viewport_height: usize, scroll_offset: usize) -> usize {
    if cursor < scroll_offset {
        cursor
    } else if cursor >= scroll_offset + viewport_height {
        cursor + 1 - viewport_height
    } else {
        scroll_offset
    }
}

/// The scroll offset that centers `cursor` within `viewport_height` rows —
/// used when a jump (go-to-definition/references, `]d`/`[d`) lands the
/// cursor somewhere off-screen, so the destination appears with context on
/// both sides rather than pinned to whichever edge ordinary movement's
/// [`clamp_scroll`] would leave it at.
pub fn center(cursor: usize, viewport_height: usize) -> usize {
    cursor.saturating_sub(viewport_height / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_page_is_never_zero_even_in_a_one_row_viewport() {
        assert_eq!(half_page(0), 1);
        assert_eq!(half_page(1), 1);
        assert_eq!(half_page(10), 5);
    }

    #[test]
    fn clamp_scroll_leaves_offset_untouched_when_cursor_already_visible() {
        assert_eq!(clamp_scroll(5, 10, 2), 2);
    }

    #[test]
    fn clamp_scroll_pulls_offset_down_to_cursor_when_cursor_moved_above_it() {
        assert_eq!(clamp_scroll(1, 10, 5), 1);
    }

    #[test]
    fn clamp_scroll_pushes_offset_forward_when_cursor_moved_past_viewport_bottom() {
        assert_eq!(clamp_scroll(12, 5, 0), 8);
    }

    #[test]
    fn center_puts_the_cursor_at_the_viewport_midpoint() {
        assert_eq!(center(20, 10), 15);
    }

    #[test]
    fn center_clamps_to_zero_near_the_top_of_the_file() {
        assert_eq!(center(2, 10), 0);
    }
}
