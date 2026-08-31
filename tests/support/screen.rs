//! Small, focused readers over a [`vt100::Screen`] for assertions the plain
//! `contents()`/`wait_for_text` string view can't answer — cell-level
//! attributes (underline, double-width) and "how many wrapped hint rows are
//! there right now." Kept separate from `harness` since none of this needs
//! a live [`Harness`](super::Harness); every function here is a pure
//! function of an already-parsed [`vt100::Screen`].

/// The bullet [`crate::ui::hints::LINE_PREFIX`] (a private constant in the
/// binary crate, so this is a second source of truth for the character —
/// U+00B7 MIDDLE DOT) every wrapped hint row starts a *line* with. The
/// top info line also uses "· " as an inter-item separator, but never as
/// the line's own first character, which is what makes "starts with · "
/// (after trimming leading spaces) an unambiguous way to count hint rows
/// specifically rather than every "·" on screen.
const HINT_BULLET: char = '\u{00B7}';

/// How many wrapped status-bar hint rows are currently on screen — 0 if the
/// hint list fit entirely without needing [`crate::ui::hints::wrap_items`]
/// to start a second line (this suite never spawns a terminal narrow enough
/// for that), otherwise one per line `hints::render_lines` produced.
pub fn hint_line_count(screen_contents: &str) -> usize {
    screen_contents
        .lines()
        .filter(|line| line.trim_start().starts_with(HINT_BULLET))
        .count()
}

/// `(row, col)` of every cell the terminal is currently rendering
/// underlined — in `diff_view`/`file_view`, exactly the active symbol's
/// span (see `diff_view::render`'s `mark_range(... Modifier::UNDERLINED)`
/// call), so this is what `Action::NextSymbol`/`PrevSymbol` (`l`/`h` in
/// vim, `M-f`/`M-b` in emacs — Tab/BackTab move pane focus instead as of
/// issue #13) actually moves, observable end-to-end through a real
/// terminal emulator rather than asserted against `App::active_symbol`
/// directly.
pub fn underlined_cells(screen: &vt100::Screen) -> Vec<(u16, u16)> {
    let (rows, cols) = screen.size();
    let mut cells = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            if screen.cell(row, col).is_some_and(vt100::Cell::underline) {
                cells.push((row, col));
            }
        }
    }
    cells
}

/// The text every cell in columns `[col_start, col_end)` renders, one line
/// per screen row — lets a test compare (or search) one pane's own content
/// without a substring that might also appear in a neighboring pane (a
/// sidebar file name and that same file's diff header both contain the
/// file name, for instance — or, since issue #26 made the diff pane's file
/// order match the sidebar tree's, a file name that's simply visible in
/// both at once). `col_end` is clamped to the screen's actual width so a
/// caller can always just pass an oversized upper bound (e.g. `100` on a
/// 100-column terminal) for "to the right edge."
pub fn region_text(screen: &vt100::Screen, col_start: u16, col_end: u16) -> String {
    let (rows, cols) = screen.size();
    let col_end = col_end.min(cols);
    let mut text = String::new();
    for row in 0..rows {
        for col in col_start..col_end {
            if let Some(cell) = screen.cell(row, col) {
                text.push_str(cell.contents());
            }
        }
        text.push('\n');
    }
    text
}

/// Whether any cell in `row`, within `[col_start, col_end)`, is currently
/// rendered reverse-video — issue #21's screen-level proof that a clicked
/// tree row is *selected* (`Modifier::REVERSED`, per `sidebar::render`'s
/// own doc comment on why it patches that instead of `BOLD` while `Files`
/// has focus) rather than merely "the background diff cursor's file,"
/// which renders cyan/underlined instead. `col_end` is clamped to the
/// screen's actual width, mirroring `region_text`'s own convenience for
/// an oversized upper bound.
pub fn row_has_reversed_cell(
    screen: &vt100::Screen,
    row: u16,
    col_start: u16,
    col_end: u16,
) -> bool {
    let (_, cols) = screen.size();
    let col_end = col_end.min(cols);
    (col_start..col_end).any(|col| screen.cell(row, col).is_some_and(vt100::Cell::inverse))
}

/// The text contents of every cell the terminal is currently rendering as
/// the first half of a double-width character — the CJK rendering
/// pipeline's actual end-to-end output, as opposed to the unit-level
/// `is_wide`/`display_width` tests already covering the layout math in
/// isolation.
pub fn wide_cell_contents(screen: &vt100::Screen) -> Vec<String> {
    let (rows, cols) = screen.size();
    let mut found = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col)
                && cell.is_wide()
            {
                found.push(cell.contents().to_owned());
            }
        }
    }
    found
}
