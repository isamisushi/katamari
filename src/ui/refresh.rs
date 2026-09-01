//! Watch-mode diff refresh: the pure part. Capturing "what row was the
//! cursor on" before a freshly re-parsed diff swaps in, and deciding where
//! it should land afterward, doesn't need a terminal, an LSP connection, or
//! even [`crate::ui::app::App`] itself — it's a function of two
//! [`crate::diff::DiffFile`] slices and a row index, which is what keeps it
//! testable against synthetic before/after models instead of only through a
//! real watch session. [`crate::ui::app::App::apply_refresh`] is the only
//! caller; it owns the terminal-facing consequences (clamping scroll,
//! resetting the active symbol) that this module deliberately has no
//! opinion about.
//!
//! Also home to [`PreRefreshHook`] — not part of anchor preservation at
//! all, but small enough, and specific enough to "the refresh pipeline," to
//! live alongside it rather than in its own single-trait module.

use crate::diff::{DiffFile, RenderRow};
use std::path::Path;

/// A snapshot of "what the cursor was looking at," captured just before a
/// refresh swaps in a freshly re-parsed diff so [`restore_anchor`] can
/// relocate the cursor afterward instead of leaving it at whatever row now
/// happens to occupy the same flat index — which, in a diff whose length
/// changed, is very often unrelated content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// The display path of the file the cursor's row belonged to, or `None`
    /// if there was no cursor row at all (an empty diff).
    file: Option<String>,
    /// The row's 0-based `new_line`, if it had one — `None` for a header,
    /// binary notice, or pure deletion row.
    new_line: Option<u32>,
    /// A hash of the row's text, for [`restore_anchor`] to tell "the exact
    /// same line number, but now different content" apart from "genuinely
    /// unchanged" — the signal `ui::mod` uses to decide whether an open
    /// hover/references overlay anchored to this row should survive the
    /// refresh.
    text_hash: Option<u64>,
    old_flat_index: usize,
    /// `cursor - scroll_offset` before the refresh — restored relative to
    /// the relocated row so the cursor keeps its screen-relative position
    /// rather than re-centering on every refresh. Signed, because a wheel
    /// scroll (see `App::scroll_by`) can deliberately run `scroll_offset`
    /// *past* the cursor; saturating that state to `0` here would make a
    /// background refresh or a cancelled search silently snap the viewport
    /// back onto a cursor the reviewer scrolled away from. Consumers pick
    /// their clamp by sign — see `App::restore_scroll_from_delta`.
    scroll_delta: isize,
}

/// Where [`restore_anchor`] decided the cursor should land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoredCursor {
    pub row_index: usize,
    /// Whether the landed-on row is *exactly* the row the anchor was
    /// captured from — same file, same line number, same text. `false` for
    /// every fallback tier (nearest line, first row of file, clamped
    /// index), and also for an exact line-number match whose content
    /// changed underneath it.
    pub overlay_survives: bool,
}

/// Captures an [`Anchor`] from `rows[cursor]`, for a caller about to replace
/// `files`/`rows` with a freshly parsed diff. `scroll_offset` is the
/// pre-refresh scroll position, used only to compute the cursor's
/// screen-relative row.
pub fn capture_anchor(
    files: &[DiffFile],
    rows: &[RenderRow],
    cursor: usize,
    scroll_offset: usize,
) -> Anchor {
    let row = rows.get(cursor).copied();
    let file = row.map(|r| files[row_file_idx(r)].display_path().to_owned());
    let (new_line, text) = row.map(|r| row_line_and_text(files, r)).unwrap_or_default();
    Anchor {
        file,
        new_line,
        text_hash: text.map(hash_text),
        old_flat_index: cursor,
        scroll_delta: cursor as isize - scroll_offset as isize,
    }
}

/// Resolves `anchor` against a freshly re-parsed `files`/`rows`, in the
/// fallback order the milestone spec calls for: the exact same
/// `(file, new_line)` if a row there still exists (regardless of whether
/// its content is what the anchor remembers — see [`RestoredCursor::overlay_survives`]),
/// else the same file's row whose `new_line` is numerically closest, else
/// that file's first row, else the old flat index clamped into the new
/// row count. An empty `rows` always resolves to index `0` with nothing
/// surviving — callers must check `rows.is_empty()` themselves before
/// trusting `row_index` as a real position.
pub fn restore_anchor(files: &[DiffFile], rows: &[RenderRow], anchor: &Anchor) -> RestoredCursor {
    if rows.is_empty() {
        return RestoredCursor {
            row_index: 0,
            overlay_survives: false,
        };
    }

    if let (Some(file), Some(line)) = (anchor.file.as_deref(), anchor.new_line) {
        if let Some(idx) = find_exact_line(files, rows, file, line) {
            let (_, text) = row_line_and_text(files, rows[idx]);
            let overlay_survives = text.map(hash_text) == anchor.text_hash;
            return RestoredCursor {
                row_index: idx,
                overlay_survives,
            };
        }
        if let Some(idx) = find_nearest_line(files, rows, file, line) {
            return RestoredCursor {
                row_index: idx,
                overlay_survives: false,
            };
        }
    }

    if let Some(file) = anchor.file.as_deref()
        && let Some(idx) = find_first_row_of_file(files, rows, file)
    {
        return RestoredCursor {
            row_index: idx,
            overlay_survives: false,
        };
    }

    RestoredCursor {
        row_index: anchor.old_flat_index.min(rows.len() - 1),
        overlay_survives: false,
    }
}

/// `cursor - scroll_offset` as [`capture_anchor`] recorded it (negative
/// when a wheel scroll had run the viewport past the cursor — see
/// [`Anchor::scroll_delta`]'s docs), for a caller to reapply once it has
/// decided the restored cursor's row index — deliberately not folded into
/// [`restore_anchor`] itself, since clamping the result against a real
/// viewport height is a rendering concern this module has no business
/// doing (see [`crate::ui::scroll`]).
pub fn scroll_delta(anchor: &Anchor) -> isize {
    anchor.scroll_delta
}

fn row_file_idx(row: RenderRow) -> usize {
    match row {
        RenderRow::FileHeader { file_idx }
        | RenderRow::BinaryNotice { file_idx }
        | RenderRow::HunkHeader { file_idx, .. }
        | RenderRow::Line { file_idx, .. }
        | RenderRow::Gap { file_idx, .. }
        | RenderRow::ReviewedHunk { file_idx, .. } => file_idx,
    }
}

/// `(new_line, text)` for a `Line` row, `(None, None)` for a header/binary
/// notice — but a synthetic `(Some(gap's starting new-side line), Some(""))`
/// for a `Gap` row rather than the same `(None, None)` a header gets: a gap
/// has a real new-side line number even though it has no text of its own,
/// and reporting it here is what lets [`restore_anchor`] fall through to
/// its nearest-line tier for a refresh whose cursor sat on a fold row,
/// instead of degrading all the way to "jump to the file header" the way a
/// `(None, None)` anchor would.
fn row_line_and_text(files: &[DiffFile], row: RenderRow) -> (Option<u32>, Option<&str>) {
    match row {
        RenderRow::Line {
            file_idx,
            hunk_idx,
            row_idx,
        } => {
            let r = &files[file_idx].hunks[hunk_idx].rows[row_idx];
            (r.new_line, Some(r.text.as_str()))
        }
        RenderRow::Gap { file_idx, gap_idx } => {
            let gap = crate::diff::file_gaps(&files[file_idx])
                .into_iter()
                .nth(gap_idx);
            match gap {
                Some(gap) => (Some(gap.new_start), Some("")),
                None => (None, None),
            }
        }
        // As the `Gap` arm above: a collapsed reviewed-hunk marker has a
        // real new-side starting line even though it renders no line text
        // of its own — reporting it here lets a refresh whose cursor sat on
        // the marker degrade to "nearest line" instead of "jump to the file
        // header."
        RenderRow::ReviewedHunk { file_idx, hunk_idx } => {
            match files.get(file_idx).and_then(|f| f.hunks.get(hunk_idx)) {
                Some(hunk) => (Some(hunk.new_start), Some("")),
                None => (None, None),
            }
        }
        RenderRow::FileHeader { .. }
        | RenderRow::BinaryNotice { .. }
        | RenderRow::HunkHeader { .. } => (None, None),
    }
}

/// `pub(crate)` (not just private) because [`crate::ui::app::App::collapse_fold_at_cursor`]
/// needs this exact "flat row whose `(file, new_line)` matches" search too
/// — sharing it there means a future fix here (e.g. around renamed files or
/// duplicate display paths) reaches both callers instead of only this
/// module's own [`restore_anchor`].
pub(crate) fn find_exact_line(
    files: &[DiffFile],
    rows: &[RenderRow],
    file: &str,
    line: u32,
) -> Option<usize> {
    rows.iter().position(|&row| {
        files[row_file_idx(row)].display_path() == file
            && row_line_and_text(files, row).0 == Some(line)
    })
}

/// The same file's `Line` row whose `new_line` is numerically closest to
/// `line` — ties broken toward whichever occurs first, matching
/// [`Iterator::min_by_key`]'s documented tie-breaking.
///
/// `pub(crate)` alongside [`find_exact_line`] for the same reason: shared
/// by [`locate_in_diff`] below.
pub(crate) fn find_nearest_line(
    files: &[DiffFile],
    rows: &[RenderRow],
    file: &str,
    line: u32,
) -> Option<usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(idx, &row)| {
            if files[row_file_idx(row)].display_path() != file {
                return None;
            }
            let (new_line, _) = row_line_and_text(files, row);
            new_line.map(|l| (idx, l.abs_diff(line)))
        })
        .min_by_key(|&(_, distance)| distance)
        .map(|(idx, _)| idx)
}

/// `pub(crate)` for [`locate_in_diff`] below, and for a structural (no-line)
/// jump target — a diff file header, say — whose row is exactly this: the
/// first row belonging to `file`, whatever kind it is.
pub(crate) fn find_first_row_of_file(
    files: &[DiffFile],
    rows: &[RenderRow],
    file: &str,
) -> Option<usize> {
    rows.iter()
        .position(|&row| files[row_file_idx(row)].display_path() == file)
}

/// Resolves a [`crate::ui::navigation::JumpEntry`]-style target (`file` as a
/// diff display path, `line` in the same 0-based LSP convention
/// [`JumpEntry::line`](crate::ui::navigation::JumpEntry) uses) against a
/// diff's `files`/`rows`, in the same fallback order [`restore_anchor`]
/// already established for a watch refresh's moved anchor: exact
/// `(file, line)` match, else that file's nearest remaining line, else that
/// file's first row (a `FileHeader`, in practice — also the only tier a
/// `None` `line` ever reaches, since a structural target has no line to
/// look for or drift from). Unlike [`restore_anchor`], this is a plain
/// existence check with no clamp-to-something tier: `file` absent from this
/// diff resolves to `None`, not a fallback guess — [`navigate_to`] needs to
/// tell "already showing this" apart from "not part of this diff at all"
/// cleanly, and a clamp tier that always succeeds would blur that.
///
/// [`DiffRow::new_line`] is 1-based (straight from the parsed diff text);
/// `line` here is 0-based, so it's converted once at this boundary rather
/// than teaching [`find_exact_line`]/[`find_nearest_line`] a second
/// numbering.
///
/// [`navigate_to`]: crate::ui::navigation::navigate_to
/// [`DiffRow::new_line`]: crate::diff::DiffRow::new_line
pub(crate) fn locate_in_diff(
    files: &[DiffFile],
    rows: &[RenderRow],
    file: &str,
    line: Option<u32>,
) -> Option<usize> {
    if let Some(line) = line {
        let one_based = line.saturating_add(1);
        if let Some(idx) = find_exact_line(files, rows, file, one_based) {
            return Some(idx);
        }
        if let Some(idx) = find_nearest_line(files, rows, file, one_based) {
            return Some(idx);
        }
    }
    find_first_row_of_file(files, rows, file)
}

/// [`locate_in_diff`] without the drift-tolerance tiers: an exact
/// `(file, line)` row or nothing (a structural `None` target still resolves
/// to the file's first row — there is no "exact line" for a header to miss).
/// A *fresh* definition/reference jump carries a position the language
/// server just resolved; if this diff doesn't render that exact line, the
/// honest answer is the real file (a pushed `FileView` at the requested
/// line), not whichever rendered row happens to be numerically nearest —
/// possibly hundreds of lines away in an unrelated hunk, shown with no hint
/// that the landing is approximate. The tolerant tiers exist for history
/// returns to *remembered* positions the content may have drifted under —
/// see [`crate::ui::navigation::navigate_to`] for which caller gets which.
pub(crate) fn locate_exact_in_diff(
    files: &[DiffFile],
    rows: &[RenderRow],
    file: &str,
    line: Option<u32>,
) -> Option<usize> {
    match line {
        Some(line) => find_exact_line(files, rows, file, line.saturating_add(1)),
        None => find_first_row_of_file(files, rows, file),
    }
}

fn hash_text(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// M5 seam: invoked once at the start of every watch-triggered refresh,
/// before the diff is re-run — the one place a future jj-backed session can
/// insert a `jj util snapshot` (or equivalent working-copy commit) without
/// restructuring anything else in the refresh pipeline. Takes the repo root
/// being refreshed, the only context a snapshot operation needs.
///
/// `Send` because the hook is constructed wherever the CLI wires up watch
/// mode and then handed to the event loop that calls it — today that's a
/// same-thread handoff, but the bound keeps a hook that itself spawns work
/// honest about crossing a thread boundary rather than relying on it never
/// happening.
pub trait PreRefreshHook: Send {
    fn before_refresh(&self, repo_root: &Path);
}

/// The default hook: a plain git working tree needs no pre-refresh step,
/// since `git diff` always reads it directly. Every live-refresh session uses
/// this until M5 supplies a real jj-backed implementation.
pub struct NoopPreRefreshHook;

impl PreRefreshHook for NoopPreRefreshHook {
    fn before_refresh(&self, _repo_root: &Path) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffHunk, DiffLineKind, DiffRow, flatten};

    /// A one-hunk file with `lines.len()` context rows, numbered `1..=len`
    /// on both sides (old and new) — the common shape these tests build
    /// before/after models out of.
    fn context_file(path: &str, lines: &[&str]) -> DiffFile {
        let rows = lines
            .iter()
            .enumerate()
            .map(|(i, text)| DiffRow {
                kind: DiffLineKind::Context,
                text: (*text).to_owned(),
                old_line: Some(i as u32 + 1),
                new_line: Some(i as u32 + 1),
            })
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
                // These tests are about anchor preservation, not fold
                // rows — no trailing gap, so every row `flatten` produces
                // is one of `lines`.
                known_eof: true,
                rows,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn exact_line_number_survives_when_content_is_unchanged() {
        let before = vec![context_file("a.rs", &["one", "two", "three"])];
        let before_rows = flatten(&before);
        // Row 0 is the file header; row 2 is "two" (new_line 2).
        let cursor = before_rows
            .iter()
            .position(|r| matches!(r, RenderRow::Line { row_idx: 1, .. }))
            .unwrap();
        let anchor = capture_anchor(&before, &before_rows, cursor, cursor);

        // Refresh with no changes at all.
        let after = before.clone();
        let after_rows = flatten(&after);
        let restored = restore_anchor(&after, &after_rows, &anchor);

        assert_eq!(restored.row_index, cursor);
        assert!(
            restored.overlay_survives,
            "unchanged content at the same line must survive"
        );
    }

    /// A cursor sitting on a collapsed `RenderRow::ReviewedHunk` marker
    /// (never produced by `flatten` itself — this simulates what
    /// `App::rederive`'s collapse pass leaves behind) must still capture a
    /// real `new_line` from the hunk's own `new_start`, so a refresh lands
    /// on the nearest surviving line rather than falling all the way back
    /// to the file header the way a `(None, None)` anchor would.
    #[test]
    fn capture_anchor_on_a_reviewed_hunk_marker_degrades_to_its_hunks_new_start_line() {
        let before = vec![context_file("a.rs", &["one", "two", "three"])];
        let before_rows = vec![
            RenderRow::FileHeader { file_idx: 0 },
            RenderRow::ReviewedHunk {
                file_idx: 0,
                hunk_idx: 0,
            },
        ];
        let anchor = capture_anchor(&before, &before_rows, 1, 1);

        // "two" edited; "one" (new_line 1, matching the hunk's new_start)
        // is untouched.
        let after = vec![context_file("a.rs", &["one", "TWO EDITED", "three"])];
        let after_rows = flatten(&after);
        let restored = restore_anchor(&after, &after_rows, &anchor);

        let RenderRow::Line { row_idx, .. } = after_rows[restored.row_index] else {
            panic!("expected a Line row");
        };
        assert_eq!(
            after[0].hunks[0].rows[row_idx].new_line,
            Some(1),
            "lands on the hunk's own new_start line, not the file header"
        );
    }

    /// A line inserted above the cursor's row shifts every following row's
    /// `new_line` down by one. Anchor preservation is deliberately
    /// line-number-based (per the milestone spec's documented fallback
    /// order), not content-tracking: the cursor lands on whatever row now
    /// sits at the *same* line number, and the overlay is reported as not
    /// surviving since that row's text is no longer what was there before.
    #[test]
    fn line_inserted_above_cursor_lands_on_the_same_line_number_with_different_content() {
        let before = vec![context_file("a.rs", &["one", "two", "three"])];
        let before_rows = flatten(&before);
        let cursor = before_rows
            .iter()
            .position(|r| matches!(r, RenderRow::Line { row_idx: 1, .. })) // "two", new_line 2
            .unwrap();
        let anchor = capture_anchor(&before, &before_rows, cursor, cursor);

        let after = vec![context_file("a.rs", &["zero", "one", "two", "three"])];
        let after_rows = flatten(&after);
        let restored = restore_anchor(&after, &after_rows, &anchor);

        // new_line 2 is now occupied by "one", not "two".
        let landed = after_rows[restored.row_index];
        let RenderRow::Line { row_idx, .. } = landed else {
            panic!("expected a line row");
        };
        assert_eq!(after[0].hunks[0].rows[row_idx].text, "one");
        assert!(
            !restored.overlay_survives,
            "the row at the anchored line number has different content now"
        );
    }

    /// A whole hunk disappearing (e.g. its changes were fully reverted)
    /// leaves the file present but with no row at the cursor's old line
    /// number anywhere in it — anchor preservation falls back to the
    /// nearest remaining line in the same file.
    #[test]
    fn hunk_removed_falls_back_to_the_nearest_line_in_the_same_file() {
        let mut before = context_file("a.rs", &["one", "two", "three"]);
        before.hunks.push(DiffHunk {
            old_start: 10,
            old_lines: 2,
            new_start: 10,
            new_lines: 2,
            header: String::new(),
            // Also not testing fold rows — no trailing gap on this now-last
            // hunk either.
            known_eof: true,
            rows: vec![
                DiffRow {
                    kind: DiffLineKind::Context,
                    text: "ten".to_owned(),
                    old_line: Some(10),
                    new_line: Some(10),
                },
                DiffRow {
                    kind: DiffLineKind::Context,
                    text: "eleven".to_owned(),
                    old_line: Some(11),
                    new_line: Some(11),
                },
            ],
        });
        let before = vec![before];
        let before_rows = flatten(&before);
        // Cursor on "eleven" (new_line 11), in the second hunk.
        let cursor = before_rows
            .iter()
            .position(|r| {
                matches!(
                    r,
                    RenderRow::Line {
                        hunk_idx: 1,
                        row_idx: 1,
                        ..
                    }
                )
            })
            .unwrap();
        let anchor = capture_anchor(&before, &before_rows, cursor, cursor);

        // After: second hunk is gone entirely; only the first remains.
        let after = vec![context_file("a.rs", &["one", "two", "three"])];
        let after_rows = flatten(&after);
        let restored = restore_anchor(&after, &after_rows, &anchor);

        // Nearest remaining new_line to 11 is "three" at new_line 3.
        let RenderRow::Line { row_idx, .. } = after_rows[restored.row_index] else {
            panic!("expected a line row");
        };
        assert_eq!(after[0].hunks[0].rows[row_idx].text, "three");
        assert!(!restored.overlay_survives);
    }

    /// The file the cursor was in disappears from the diff entirely (its
    /// only change was reverted, or it was unstaged out of scope) — nothing
    /// in the new diff has that display path at all, so preservation falls
    /// all the way back to a clamped flat index.
    #[test]
    fn file_disappeared_falls_back_to_a_clamped_flat_index() {
        let before = vec![
            context_file("a.rs", &["a-one"]),
            context_file("b.rs", &["b-one", "b-two"]),
        ];
        let before_rows = flatten(&before);
        // Cursor on b.rs's "b-two".
        let cursor = before_rows
            .iter()
            .position(|r| {
                matches!(
                    r,
                    RenderRow::Line {
                        file_idx: 1,
                        row_idx: 1,
                        ..
                    }
                )
            })
            .unwrap();
        let anchor = capture_anchor(&before, &before_rows, cursor, cursor);

        // After: b.rs is gone; only a.rs remains, much shorter than the
        // old flat index pointed into.
        let after = vec![context_file("a.rs", &["a-one"])];
        let after_rows = flatten(&after);
        let restored = restore_anchor(&after, &after_rows, &anchor);

        assert_eq!(
            restored.row_index,
            after_rows.len() - 1,
            "clamped to the last valid row"
        );
        assert!(!restored.overlay_survives);
    }

    #[test]
    fn empty_after_diff_restores_to_index_zero_without_panicking() {
        let before = vec![context_file("a.rs", &["one"])];
        let before_rows = flatten(&before);
        let anchor = capture_anchor(&before, &before_rows, 0, 0);

        let after: Vec<DiffFile> = Vec::new();
        let after_rows = flatten(&after);
        let restored = restore_anchor(&after, &after_rows, &anchor);

        assert_eq!(restored.row_index, 0);
        assert!(!restored.overlay_survives);
    }

    #[test]
    fn scroll_delta_reports_the_captured_cursor_relative_offset() {
        let before = vec![context_file("a.rs", &["one", "two", "three"])];
        let before_rows = flatten(&before);
        let cursor = 3; // some row a few past the top
        let anchor = capture_anchor(&before, &before_rows, cursor, 1);
        assert_eq!(scroll_delta(&anchor), 2);
    }

    #[test]
    fn scroll_delta_goes_negative_when_the_viewport_ran_past_the_cursor() {
        // The wheel-decoupled state (`App::scroll_by`): scroll_offset can
        // exceed the cursor, and the anchor must represent that distance
        // rather than saturating it to 0 — otherwise a refresh restores
        // the cursor pinned to the top row, discarding the wheel position.
        let before = vec![context_file("a.rs", &["one", "two", "three"])];
        let before_rows = flatten(&before);
        let anchor = capture_anchor(&before, &before_rows, 1, 4);
        assert_eq!(scroll_delta(&anchor), -3);
    }

    // ---- locate_in_diff -------------------------------------------------

    #[test]
    fn locate_in_diff_finds_the_exact_line() {
        let files = vec![context_file("a.rs", &["one", "two", "three"])];
        let rows = flatten(&files);
        // "two" is new_line 2 (1-based), i.e. 0-based line 1.
        let idx = locate_in_diff(&files, &rows, "a.rs", Some(1)).unwrap();
        let RenderRow::Line { row_idx, .. } = rows[idx] else {
            panic!("expected a line row");
        };
        assert_eq!(files[0].hunks[0].rows[row_idx].text, "two");
    }

    #[test]
    fn locate_in_diff_falls_back_to_the_nearest_line_when_the_exact_one_is_gone() {
        let files = vec![context_file("a.rs", &["one", "two", "three"])];
        let rows = flatten(&files);
        // 0-based line 10 (new_line 11) doesn't exist — nearest is "three"
        // (new_line 3, 0-based line 2).
        let idx = locate_in_diff(&files, &rows, "a.rs", Some(10)).unwrap();
        let RenderRow::Line { row_idx, .. } = rows[idx] else {
            panic!("expected a line row");
        };
        assert_eq!(files[0].hunks[0].rows[row_idx].text, "three");
    }

    #[test]
    fn locate_in_diff_with_no_line_lands_on_the_files_first_row() {
        let files = vec![
            context_file("a.rs", &["a-one"]),
            context_file("b.rs", &["b-one", "b-two"]),
        ];
        let rows = flatten(&files);
        let idx = locate_in_diff(&files, &rows, "b.rs", None).unwrap();
        assert!(
            matches!(rows[idx], RenderRow::FileHeader { file_idx: 1 }),
            "expected b.rs's own file header, got {:?}",
            rows[idx]
        );
    }

    #[test]
    fn locate_in_diff_reports_none_for_a_file_absent_from_this_diff() {
        let files = vec![context_file("a.rs", &["one"])];
        let rows = flatten(&files);
        assert_eq!(locate_in_diff(&files, &rows, "missing.rs", Some(0)), None);
        assert_eq!(locate_in_diff(&files, &rows, "missing.rs", None), None);
    }
}
