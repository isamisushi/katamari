//! Issue #5's `/` search: pure match computation, smartcase, and the
//! next/prev-with-wraparound stepping rule — everything about "where are
//! the matches and which one is current" that doesn't need a terminal, an
//! `App`, or even `ratatui`, matching every other pure state-transition
//! module in this codebase (`ui::refresh`, `ui::scroll`).
//!
//! [`Match`] stores a **byte offset range** into its row's raw, unwrapped
//! text — not a display column. Two things forced that choice over the
//! display-column-at-scan-time approach [`crate::ui::symbols::scan`] uses
//! for the active-symbol highlight: a match has to survive a `[ui] wrap`
//! toggle unchanged (byte offsets don't depend on where a line happens to
//! wrap this frame; a display column computed once and cached would), and
//! literal substring search is naturally a byte/char operation to begin
//! with (there's no tab-stop or double-width-character math involved in
//! *finding* a match, only in *positioning* it once found). The byte range
//! is converted to a display column only at render/jump time, via
//! [`crate::diff::ColumnMap::utf8_to_display`] — the same byte→display leg
//! `crate::ui::navigation::location_to_target` already uses for a
//! go-to-definition response's LSP position, applied here to a match
//! instead. See `crate::ui::app::App::next_match`/`recompute_search_live`
//! and `crate::ui::diff_view::content_line` for the two call sites that run
//! that conversion.
//!
//! [`SearchPromptState`]/[`SearchInput`]/[`handle_prompt_key`] are the `/`
//! prompt's own raw-key-bypass overlay. `SearchInput` is a
//! [`crate::ui::compose::LineInput`] type alias, the same shared buffer
//! [`crate::ui::scope_menu::RevisionInput`] aliases too — see that type's
//! docs for why the buffer itself lives in `compose` rather than being
//! redefined here; only [`handle_prompt_key`]'s key dispatch (Esc cancels
//! rather than "back to the menu list", Enter confirms rather than
//! submits) is specific to this module.

use crate::diff::{DiffFile, RenderRow};
use crate::ui::refresh::Anchor;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// One substring occurrence: `row_idx` is the flat index into
/// [`crate::ui::app::App::rows`] (only ever [`RenderRow::Line`] rows are
/// searched — see [`compute_matches`]), `start`/`end` are UTF-8 byte
/// offsets `[start, end)` into that row's raw `DiffRow::text` — see this
/// module's docs on why bytes, not a display column, are what's stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub row_idx: usize,
    pub start: usize,
    pub end: usize,
}

/// The active search: what [`crate::ui::diff_view`] marks on every row, and
/// what `n`/`N` cycle through. A live incremental prompt (before Enter) and
/// a confirmed search (after) share this one shape rather than needing two
/// types — a confirmed search *is* exactly what an incremental one looked
/// like the instant Enter locked it in, see
/// `crate::ui::app::App::confirm_search`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHighlight {
    pub query: String,
    pub matches: Vec<Match>,
    /// Index into `matches` — always in bounds when `matches` is non-empty
    /// (every constructor here clamps it via [`nearest_match_index`]'s
    /// `unwrap_or(0)` fallback); meaningless (but harmless) when `matches`
    /// is empty.
    pub current: usize,
    /// Whether the confirmed query's matches should actually be *drawn* —
    /// kept separate from whether there's a confirmed query to repeat at
    /// all, so a bare `Esc` in the normal diff view (vim's `:noh`, see
    /// `crate::ui::app::App::clear_search`) can suppress the highlight
    /// without discarding `query`/`matches`/`current` themselves. Real
    /// vim's `:nohlsearch` only hides highlighting; the pattern stays live
    /// in the search register, so `n`/`N` keep working right afterward, and
    /// vim re-enables highlighting the moment you search again (see
    /// `crate::ui::app::App::jump_to_match`, which flips this back to
    /// `true` on every successful step). Always `true` for a freshly
    /// computed or reconfirmed search — only `App::clear_search` ever sets
    /// it `false`.
    pub highlight_visible: bool,
}

/// Whether `query` should match case-insensitively — vim's own `smartcase`
/// rule: an all-lowercase query matches either case ("you didn't bother
/// capitalizing, so you don't care"), but the moment it contains even one
/// uppercase letter the search becomes case-sensitive ("you typed a
/// capital on purpose"). Checked once per `query`, not per candidate
/// character, since it depends only on what was typed, never on what's
/// being searched.
fn is_case_sensitive(query: &str) -> bool {
    query.chars().any(char::is_uppercase)
}

/// Whether haystack-char `a` and needle-char `b` count as the same
/// character for matching purposes: always for an exact match, or also
/// when their full (possibly multi-codepoint) lowercase forms agree when
/// `case_sensitive` is false — comparing the *iterators* `char::to_lowercase`
/// produces, rather than lowercasing either string as a whole and
/// re-deriving byte offsets from the folded copy, which for the handful of
/// characters whose lowercase form is more than one codepoint (Turkish
/// dotted İ, among others) would drift out of step with the *original*
/// text's byte positions — exactly the offsets [`Match`] has to report.
fn chars_match(a: char, b: char, case_sensitive: bool) -> bool {
    a == b || (!case_sensitive && a.to_lowercase().eq(b.to_lowercase()))
}

/// Every non-overlapping occurrence of `needle` in `haystack`, as byte
/// ranges, scanning left to right and advancing past each match (so three
/// occurrences of `"aa"` in `"aaaa"` find two, not three — vim's own `/`
/// search advances the same way rather than reporting overlapping hits).
/// `[]` for an empty `needle` — there's no sane "every position" answer a
/// caller would want, and [`compute_matches`] never calls this with one
/// (see its own empty-query guard).
fn find_all(haystack: &str, needle: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let hay: Vec<(usize, char)> = haystack.char_indices().collect();
    let needle_chars: Vec<char> = needle.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    'outer: while i + needle_chars.len() <= hay.len() {
        for (offset, &nc) in needle_chars.iter().enumerate() {
            if !chars_match(hay[i + offset].1, nc, case_sensitive) {
                i += 1;
                continue 'outer;
            }
        }
        let start = hay[i].0;
        let end = hay
            .get(i + needle_chars.len())
            .map_or(haystack.len(), |&(byte, _)| byte);
        out.push((start, end));
        i += needle_chars.len(); // non-overlapping: resume right after this match
    }
    out
}

/// Every match of `query` across `rows`, in `rows` order (file → hunk →
/// line, per [`crate::diff::flatten`]'s own order — so cross-file ordering
/// falls out for free rather than needing a second sort) and, within one
/// row, left to right. Only [`RenderRow::Line`] rows are searched — a file
/// header, hunk header, binary notice, or fold row has no line of source
/// text to match against (see [`RenderRow`]'s docs on what each variant
/// stands for); a fold row in particular means hidden (folded) context
/// stays unsearched until `z o` replaces it with real `Line` rows, which is
/// what makes [`crate::ui::app::App`]'s post-fold recompute meaningful
/// rather than a no-op. `[]` for an empty `query`.
pub fn compute_matches(files: &[DiffFile], rows: &[RenderRow], query: &str) -> Vec<Match> {
    if query.is_empty() {
        return Vec::new();
    }
    let case_sensitive = is_case_sensitive(query);
    let mut out = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        let RenderRow::Line {
            file_idx,
            hunk_idx,
            row_idx: line_idx,
        } = row
        else {
            continue;
        };
        let text = &files[*file_idx].hunks[*hunk_idx].rows[*line_idx].text;
        for (start, end) in find_all(text, query, case_sensitive) {
            out.push(Match {
                row_idx,
                start,
                end,
            });
        }
    }
    out
}

/// The "which match is closest to a given row" rule shared by
/// [`compute_search`] (closest to where the incremental prompt started) and
/// `crate::ui::app::App::recompute_search` (closest to the cursor after a
/// fold toggle or a watch refresh rebuilt the rows out from under a
/// confirmed search) — factored out so both recompute paths agree on one
/// definition of "nearest" and so it's unit-testable without either an
/// `App` or a live prompt. The first match at-or-after `cursor_row`; when
/// every match sits *before* it, wraps to the very first match overall
/// (index `0`) rather than reporting "none" — there's always *some* match
/// to point `current` at once this is called with a non-empty `matches`,
/// and wrapping here means the origin lands on a real match rather than an
/// arbitrary one, mirroring `'wrapscan'` (vim's default) rather than a
/// no-wrap search. Returns `0` (not an `Option`) even for an empty
/// `matches`, since every caller already guards that case itself before
/// this would matter.
pub fn nearest_match_index(matches: &[Match], cursor_row: usize) -> usize {
    matches
        .iter()
        .position(|m| m.row_idx >= cursor_row)
        .unwrap_or(0)
}

/// The live incremental prompt's full recomputation for one keystroke:
/// every match of `query`, with `current` selected as the first one
/// at-or-after `origin_row` — vim's own incremental `/` always previews
/// from the position search *started* at, not from wherever a previous,
/// less-narrow query's preview happened to land the cursor, and that holds
/// even when `query` currently matches nothing at all: this function always
/// resolves `current` (and, when `matches` is empty, is paired with
/// [`crate::ui::app::App::recompute_search_live`]'s own explicit restore of
/// the cursor/scroll to `origin_row`) against the *origin*, never against
/// wherever a previous keystroke's preview happened to leave things (see
/// that method's docs on why `origin_row` is resolved fresh from an
/// [`Anchor`] on every call rather than trusted as a fixed row index).
/// `None` for an empty `query` — nothing typed yet (the prompt just opened,
/// or every character was deleted) means no search to show at all, matching
/// Esc's own "clear the highlight entirely" behavior rather than a `Some`
/// holding a match-everything empty needle.
pub fn compute_search(
    files: &[DiffFile],
    rows: &[RenderRow],
    query: &str,
    origin_row: usize,
) -> Option<SearchHighlight> {
    if query.is_empty() {
        return None;
    }
    let matches = compute_matches(files, rows, query);
    let current = nearest_match_index(&matches, origin_row);
    Some(SearchHighlight {
        query: query.to_owned(),
        matches,
        current,
        highlight_visible: true,
    })
}

/// `n`/`N`'s pure stepping rule: the next/previous index into a
/// `len`-long match list from `current`, wrapping around either end.
/// [`crate::ui::app::App::next_match`]/[`crate::ui::app::App::prev_match`]
/// are thin wrappers that call this and then do the actual cursor jump —
/// the one impure half (`App::jump_cursor_to`) that has no business living
/// in this otherwise `&mut App`-free module. `None` when `len == 0`
/// (nothing to step to) rather than assuming a caller already checked, so
/// `App`'s wrapper and this function's own tests share one guard. The
/// returned `bool` is whether this step wrapped around either end — with
/// exactly one match, every step "wraps" (it's simultaneously the first and
/// last), which is the correct, if slightly odd-looking, answer: cycling
/// through a single match really does return to itself every time.
pub fn step(current: usize, len: usize, forward: bool) -> Option<(usize, bool)> {
    if len == 0 {
        return None;
    }
    Some(if forward {
        if current + 1 >= len {
            (0, true)
        } else {
            (current + 1, false)
        }
    } else if current == 0 {
        (len - 1, true)
    } else {
        (current - 1, false)
    })
}

/// The `/` prompt's single-line text buffer — [`crate::ui::compose::LineInput`]
/// under its own name here rather than a redefinition: a search query's
/// buffer shape (char-indexed insert/backspace/move-left/move-right) has
/// nothing to do with search *semantics*, it's the exact same "one line of
/// free-form text" problem [`crate::ui::scope_menu::RevisionInput`] already
/// solves, so both alias the one shared type instead of each maintaining
/// their own copy of the same ~40 lines. See `LineInput`'s own docs for the
/// buffer itself; only this module's [`handle_prompt_key`] dispatch is
/// specific to a search prompt.
pub type SearchInput = crate::ui::compose::LineInput;

/// What [`handle_prompt_key`] decided one key press should do, beyond
/// editing the buffer itself — the single-line sibling of
/// [`crate::ui::scope_menu::RevisionInputOutcome`], but with `Confirm`
/// carrying no payload (unlike `RevisionInputOutcome::Submit`): the caller
/// already has the current text via [`SearchInput::text`] on the
/// [`SearchPromptState`] it owns, so there's nothing this needs to hand
/// back that isn't already sitting right there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPromptOutcome {
    Continue,
    /// `Enter`: lock the current query in as the confirmed search — see
    /// `crate::ui::app::App::confirm_search`.
    Confirm,
    /// `Esc`: discard the query, restore the pre-search cursor/scroll, and
    /// clear the highlight — see `crate::ui::app::App::cancel_search`.
    Cancel,
}

/// Applies one raw terminal key event to `input`, bypassing
/// [`crate::keymap`] entirely — mirrors
/// [`crate::ui::scope_menu::handle_revision_key`]'s reasoning exactly: a
/// query like `TODO` or `fn main` can contain characters (space, `(`, `.`)
/// that would otherwise resolve to unrelated [`crate::keymap::Action`]s if
/// routed through the keymap resolver first.
pub fn handle_prompt_key(input: &mut SearchInput, key: KeyEvent) -> SearchPromptOutcome {
    match key.code {
        KeyCode::Esc => SearchPromptOutcome::Cancel,
        KeyCode::Enter => SearchPromptOutcome::Confirm,
        KeyCode::Backspace => {
            input.backspace();
            SearchPromptOutcome::Continue
        }
        KeyCode::Left => {
            input.move_left();
            SearchPromptOutcome::Continue
        }
        KeyCode::Right => {
            input.move_right();
            SearchPromptOutcome::Continue
        }
        // As `compose::handle_key`/`handle_revision_key`: a stray
        // control/alt-modified char (habitual `C-a`/`C-e` etc.) is left
        // alone rather than inserted literally.
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            input.insert_char(c);
            SearchPromptOutcome::Continue
        }
        _ => SearchPromptOutcome::Continue,
    }
}

/// The `/` prompt's live state while it's open: the raw-bypass overlay's
/// own buffer plus the cursor/scroll position search started from, captured
/// once (see `crate::ui::refresh::capture_anchor`) the moment `/` is
/// pressed. An [`Anchor`], not a raw `(cursor, scroll)` pair — row indices
/// go stale the instant a watch refresh rebuilds `rows` mid-prompt (see
/// `Anchor`'s own docs), which a raw pair has no way to recover from but an
/// anchor, resolved fresh via `crate::ui::refresh::restore_anchor` on every
/// recompute, does. `crate::ui::mod`'s event loop owns this the same way it
/// owns `compose`/`scope_menu`/`help` — a transient overlay, not `App`
/// state (see [`SearchHighlight`]'s docs for the confirmed half this
/// becomes on Enter, which *does* live on `App`).
pub struct SearchPromptState {
    pub input: SearchInput,
    pub origin: Anchor,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffHunk, DiffLineKind, DiffRow, flatten};
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn line(text: &str, new_line: u32) -> DiffRow {
        DiffRow {
            kind: DiffLineKind::Context,
            text: text.to_owned(),
            old_line: Some(new_line),
            new_line: Some(new_line),
        }
    }

    fn file(path: &str, lines: &[&str]) -> DiffFile {
        let rows = lines
            .iter()
            .enumerate()
            .map(|(i, text)| line(text, i as u32 + 1))
            .collect();
        DiffFile {
            old_path: Some(path.to_owned()),
            new_path: Some(path.to_owned()),
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
        }
    }

    // ---- smartcase ----------------------------------------------------

    #[test]
    fn lowercase_query_matches_lowercase_text() {
        let files = vec![file("a.txt", &["hello world"])];
        let rows = flatten(&files);
        let matches = compute_matches(&files, &rows, "hello");
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn lowercase_query_matches_uppercase_text_case_insensitively() {
        let files = vec![file("a.txt", &["HELLO world"])];
        let rows = flatten(&files);
        let matches = compute_matches(&files, &rows, "hello");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].start, 0);
        assert_eq!(matches[0].end, 5);
    }

    #[test]
    fn any_uppercase_letter_makes_the_query_case_sensitive() {
        let files = vec![file("a.txt", &["hello World"])];
        let rows = flatten(&files);
        // "World" (capital W) only matches the capitalized occurrence.
        assert_eq!(compute_matches(&files, &rows, "World").len(), 1);
        assert_eq!(compute_matches(&files, &rows, "world").len(), 1); // still matches, case-insensitive
    }

    #[test]
    fn case_sensitive_query_does_not_match_a_different_case_occurrence() {
        let files = vec![file("a.txt", &["hello world, Hello Again"])];
        let rows = flatten(&files);
        // "Hello" (capital) must not match the lowercase "hello".
        let matches = compute_matches(&files, &rows, "Hello");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].start, "hello world, ".len());
    }

    // ---- ordering -------------------------------------------------------

    #[test]
    fn multiple_matches_in_one_row_are_ordered_left_to_right() {
        let files = vec![file("a.txt", &["cat cat cat"])];
        let rows = flatten(&files);
        let matches = compute_matches(&files, &rows, "cat");
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].start, 0);
        assert_eq!(matches[1].start, 4);
        assert_eq!(matches[2].start, 8);
    }

    #[test]
    fn matches_do_not_overlap_and_advance_past_each_hit() {
        let files = vec![file("a.txt", &["aaaa"])];
        let rows = flatten(&files);
        // "aa" in "aaaa" is two non-overlapping hits (0..2, 2..4), not
        // three overlapping ones.
        let matches = compute_matches(&files, &rows, "aa");
        assert_eq!(matches.len(), 2);
        assert_eq!((matches[0].start, matches[0].end), (0, 2));
        assert_eq!((matches[1].start, matches[1].end), (2, 4));
    }

    #[test]
    fn matches_are_ordered_across_files_in_flat_row_order() {
        let files = vec![
            file("a.txt", &["needle in a"]),
            file("b.txt", &["needle in b"]),
        ];
        let rows = flatten(&files);
        let matches = compute_matches(&files, &rows, "needle");
        assert_eq!(matches.len(), 2);
        assert!(
            matches[0].row_idx < matches[1].row_idx,
            "a.txt's match must come before b.txt's in the flat row order"
        );
    }

    #[test]
    fn only_line_rows_are_searched_never_headers() {
        let files = vec![file("needle.txt", &["nothing here"])];
        let rows = flatten(&files);
        // The query matches the file's own *path* ("needle"), which only
        // ever appears in a `RenderRow::FileHeader`, never a `Line` row's
        // text — so there must be zero matches.
        let matches = compute_matches(&files, &rows, "needle");
        assert!(matches.is_empty());
    }

    // ---- zero matches -----------------------------------------------------

    #[test]
    fn a_query_present_nowhere_yields_no_matches() {
        let files = vec![file("a.txt", &["hello world"])];
        let rows = flatten(&files);
        assert!(compute_matches(&files, &rows, "xyz").is_empty());
    }

    #[test]
    fn an_empty_query_yields_no_matches_and_no_search() {
        let files = vec![file("a.txt", &["hello world"])];
        let rows = flatten(&files);
        assert!(compute_matches(&files, &rows, "").is_empty());
        assert!(compute_search(&files, &rows, "", 0).is_none());
    }

    // ---- incremental narrowing / origin jump semantics ---------------------

    #[test]
    fn compute_search_selects_the_first_match_at_or_after_the_origin_row() {
        let files = vec![file(
            "a.txt",
            &["needle one", "context", "needle two", "needle three"],
        )];
        let rows = flatten(&files);
        // Flat rows: 0 FileHeader, 1 HunkHeader, 2 "needle one" (row_idx 2),
        // 3 "context", 4 "needle two", 5 "needle three".
        let highlight = compute_search(&files, &rows, "needle", 4).unwrap();
        assert_eq!(highlight.matches.len(), 3);
        let current = highlight.matches[highlight.current];
        assert_eq!(
            current.row_idx, 4,
            "origin at row 4 should land on the match starting exactly there, not row 2's earlier one"
        );
    }

    #[test]
    fn compute_search_wraps_to_the_first_match_when_the_origin_is_past_all_of_them() {
        let files = vec![file("a.txt", &["needle one", "needle two"])];
        let rows = flatten(&files);
        // Origin row far past both matches.
        let highlight = compute_search(&files, &rows, "needle", 100).unwrap();
        assert_eq!(highlight.current, 0);
    }

    #[test]
    fn compute_search_with_zero_matches_still_returns_a_highlight_with_an_empty_list() {
        let files = vec![file("a.txt", &["hello world"])];
        let rows = flatten(&files);
        let highlight = compute_search(&files, &rows, "xyz", 0).unwrap();
        assert!(highlight.matches.is_empty());
        assert_eq!(highlight.query, "xyz");
    }

    // ---- nearest_match_index (recompute-after-rows-change) -----------------

    #[test]
    fn nearest_match_index_keeps_the_first_match_at_or_after_the_cursor() {
        let matches = vec![
            Match {
                row_idx: 2,
                start: 0,
                end: 1,
            },
            Match {
                row_idx: 8,
                start: 0,
                end: 1,
            },
        ];
        assert_eq!(nearest_match_index(&matches, 5), 1);
        assert_eq!(nearest_match_index(&matches, 0), 0);
    }

    #[test]
    fn nearest_match_index_wraps_to_the_first_when_the_cursor_is_past_every_match() {
        let matches = vec![Match {
            row_idx: 2,
            start: 0,
            end: 1,
        }];
        assert_eq!(nearest_match_index(&matches, 50), 0);
    }

    #[test]
    fn nearest_match_index_of_an_empty_list_is_zero() {
        assert_eq!(nearest_match_index(&[], 5), 0);
    }

    // ---- next/prev wraparound ----------------------------------------------

    #[test]
    fn step_forward_advances_without_wrapping_mid_list() {
        assert_eq!(step(0, 3, true), Some((1, false)));
        assert_eq!(step(1, 3, true), Some((2, false)));
    }

    #[test]
    fn step_forward_wraps_from_the_last_match_to_the_first() {
        assert_eq!(step(2, 3, true), Some((0, true)));
    }

    #[test]
    fn step_backward_retreats_without_wrapping_mid_list() {
        assert_eq!(step(2, 3, false), Some((1, false)));
    }

    #[test]
    fn step_backward_wraps_from_the_first_match_to_the_last() {
        assert_eq!(step(0, 3, false), Some((2, true)));
    }

    #[test]
    fn step_with_a_single_match_always_wraps_to_itself() {
        assert_eq!(step(0, 1, true), Some((0, true)));
        assert_eq!(step(0, 1, false), Some((0, true)));
    }

    #[test]
    fn step_with_zero_matches_is_none() {
        assert_eq!(step(0, 0, true), None);
        assert_eq!(step(0, 0, false), None);
    }

    // ---- unicode / CJK / tab match columns ----------------------------------

    #[test]
    fn a_cjk_row_reports_correct_byte_offsets_for_conversion_to_display_columns() {
        use crate::diff::ColumnMap;
        // "let 名前 = 1;" — searching for "名前" (a CJK identifier).
        let files = vec![file("a.txt", &["let 名前 = 1;"])];
        let rows = flatten(&files);
        let matches = compute_matches(&files, &rows, "名前");
        assert_eq!(matches.len(), 1);
        let m = matches[0];
        let text = "let 名前 = 1;";
        assert_eq!(&text[m.start..m.end], "名前");
        // Byte offsets convert to the display columns a CJK-aware caller
        // (diff_view's `content_line`) actually needs: "let " is 4 columns
        // wide, so "名前" starts at display column 4 and is 4 columns wide
        // itself (2 per double-width character) — matching `ColumnMap`'s
        // own CJK test fixture.
        let columns = ColumnMap::new(text);
        assert_eq!(columns.utf8_to_display(m.start), 4);
        assert_eq!(columns.utf8_to_display(m.end), 8);
    }

    #[test]
    fn a_tab_indented_row_reports_byte_offsets_that_convert_past_the_tab_stop() {
        use crate::diff::ColumnMap;
        // A leading tab (width 4 at the default tab stop) before "needle".
        let files = vec![file("a.txt", &["\tneedle"])];
        let rows = flatten(&files);
        let matches = compute_matches(&files, &rows, "needle");
        assert_eq!(matches.len(), 1);
        let m = matches[0];
        assert_eq!(m.start, 1); // right after the one-byte tab
        let columns = ColumnMap::with_tab_width("\tneedle", 4);
        // The tab expands to 4 display columns, so "needle" starts at
        // display column 4, not byte offset 1.
        assert_eq!(columns.utf8_to_display(m.start), 4);
    }

    // ---- SearchInput / handle_prompt_key -----------------------------------

    fn key(code: KeyCode) -> KeyEvent {
        key_mod(code, KeyModifiers::NONE)
    }

    fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn typed_characters_insert_at_the_cursor() {
        let mut input = SearchInput::new();
        for c in "fn main".chars() {
            input.insert_char(c);
        }
        assert_eq!(input.text(), "fn main");
        assert_eq!(input.cursor(), 7);
    }

    #[test]
    fn backspace_and_arrows_are_byte_safe_for_multi_byte_text() {
        let mut input = SearchInput::new();
        for c in "日本語".chars() {
            input.insert_char(c);
        }
        input.backspace();
        assert_eq!(input.text(), "日本");
        input.move_left();
        input.move_left();
        assert_eq!(input.cursor(), 0);
        input.insert_char('a');
        assert_eq!(input.text(), "a日本");
    }

    #[test]
    fn handle_key_esc_cancels() {
        let mut input = SearchInput::new();
        assert_eq!(
            handle_prompt_key(&mut input, key(KeyCode::Esc)),
            SearchPromptOutcome::Cancel
        );
    }

    #[test]
    fn handle_key_enter_confirms() {
        let mut input = SearchInput::new();
        input.insert_char('x');
        assert_eq!(
            handle_prompt_key(&mut input, key(KeyCode::Enter)),
            SearchPromptOutcome::Confirm
        );
    }

    #[test]
    fn handle_key_plain_char_inserts_and_continues() {
        let mut input = SearchInput::new();
        assert_eq!(
            handle_prompt_key(&mut input, key(KeyCode::Char('a'))),
            SearchPromptOutcome::Continue
        );
        assert_eq!(input.text(), "a");
    }

    #[test]
    fn handle_key_control_modified_char_is_not_inserted() {
        let mut input = SearchInput::new();
        let outcome = handle_prompt_key(
            &mut input,
            key_mod(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        assert_eq!(outcome, SearchPromptOutcome::Continue);
        assert_eq!(input.text(), "");
    }
}
