//! Converts a single line of text between the three coordinate systems that
//! meet at the cursor: terminal *display columns* (what moving the cursor
//! left/right and the gutter's alignment mean), *UTF-8 byte offsets* (how
//! Rust indexes `&str`), and *UTF-16 code-unit offsets* (what the Language
//! Server Protocol uses for `Position::character` by default — the spec
//! allows a server to negotiate UTF-8 or UTF-32 instead via
//! `positionEncodings`, but UTF-16 is the value every server must support,
//! since it's what LSP shipped with originally to match VS Code's internal
//! string representation).
//!
//! [`ColumnMap`] builds this correspondence once per line, indexed by
//! grapheme cluster (not `char`) so a multi-codepoint cluster — combining
//! marks, ZWJ emoji sequences — always converts as one unit: a display
//! column can never land "inside" a grapheme the way it could land inside a
//! `char`'s byte encoding.
//!
//! Width accounting deliberately mirrors [`crate::ui::text::truncate_to_width`]
//! exactly rather than inventing its own rule: both walk `line.graphemes(true)`
//! and size each cluster with `UnicodeWidthStr::width`. That includes how
//! `unicode-width` 0.2 treats control characters — a tab has display width 1
//! there, *not* an expansion to the next 4-column stop, because
//! `ui::diff_view` never special-cases `\t` before handing text to ratatui.
//! An earlier plan note assumed tab-stop expansion; this module matches the
//! renderer as it actually behaves instead, since a `ColumnMap` that
//! disagreed with what's on screen would defeat its own purpose. See the
//! module-level test `tab_is_one_display_column_like_diff_view_renders_it`
//! for the empirical check this claim rests on.

use crate::diff::model::{DiffFile, DiffLineKind, DiffRow};
use std::path::PathBuf;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// One grapheme cluster's position in all three coordinate systems. `_len`
/// fields (not `_end`) because that's what every caller actually wants —
/// "does this offset fall within the cluster" is a start+len comparison in
/// each system independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Segment {
    display_start: usize,
    display_width: usize,
    utf8_start: usize,
    utf8_len: usize,
    utf16_start: usize,
    utf16_len: usize,
}

/// The full correspondence for one line, built once and queried as often as
/// the cursor moves or an LSP position needs converting. Construction is
/// O(line length); every query afterward is a linear scan of the segment
/// list, which is fine at the line lengths a terminal renders (hundreds of
/// columns, not megabytes).
pub struct ColumnMap {
    segments: Vec<Segment>,
    display_len: usize,
    utf8_len: usize,
    utf16_len: usize,
}

impl ColumnMap {
    pub fn new(line_text: &str) -> Self {
        let mut segments = Vec::new();
        let (mut display_pos, mut utf8_pos, mut utf16_pos) = (0usize, 0usize, 0usize);

        for grapheme in line_text.graphemes(true) {
            let display_width = grapheme.width();
            let utf8_len = grapheme.len();
            let utf16_len: usize = grapheme.chars().map(char::len_utf16).sum();

            segments.push(Segment {
                display_start: display_pos,
                display_width,
                utf8_start: utf8_pos,
                utf8_len,
                utf16_start: utf16_pos,
                utf16_len,
            });

            display_pos += display_width;
            utf8_pos += utf8_len;
            utf16_pos += utf16_len;
        }

        Self {
            segments,
            display_len: display_pos,
            utf8_len: utf8_pos,
            utf16_len: utf16_pos,
        }
    }

    // `display_len`/`utf8_len`/`utf16_len`/`utf8_to_display`/`utf16_to_display`
    // (below) are exercised by this module's own tests but have no
    // production caller yet in M3a, which only ever converts
    // display-column-to-server-position (`display_to_utf8`/`display_to_utf16`).
    // The reverse direction — a server's byte/UTF-16 offset back to a
    // display column — is exactly what M3b's go-to-definition needs to
    // place the cursor at a returned location, so these stay rather than
    // getting deleted and re-derived later. Same convention as
    // `ViewStack::push` in `ui::view`.
    #[allow(dead_code)]
    pub fn display_len(&self) -> usize {
        self.display_len
    }

    #[allow(dead_code)]
    pub fn utf8_len(&self) -> usize {
        self.utf8_len
    }

    #[allow(dead_code)]
    pub fn utf16_len(&self) -> usize {
        self.utf16_len
    }

    /// The display column a terminal cursor sitting on `display_col` would
    /// need for its content to start at, converted to a UTF-8 byte offset
    /// into the line. A column that lands mid-way through a wide (2-cell)
    /// grapheme snaps to that grapheme's start — there is no "half a
    /// character" byte offset to give it. A column at or past the end of the
    /// line clamps to the line's total byte length, matching how a cursor
    /// allowed to sit just past the last character behaves.
    pub fn display_to_utf8(&self, display_col: usize) -> usize {
        self.find_by_display(display_col)
            .map_or(self.utf8_len, |seg| seg.utf8_start)
    }

    /// As [`Self::display_to_utf8`], but into UTF-16 code-unit offsets — the
    /// coordinate `lsp_types::Position::character` uses when the server
    /// negotiated UTF-16 (LSP's default and every server's required
    /// fallback).
    pub fn display_to_utf16(&self, display_col: usize) -> usize {
        self.find_by_display(display_col)
            .map_or(self.utf16_len, |seg| seg.utf16_start)
    }

    /// The inverse of [`Self::display_to_utf8`]: an LSP hover result or
    /// diagnostic range arrives as a UTF-8 byte offset (when UTF-8 encoding
    /// was negotiated) and needs to become a display column to place a
    /// cursor or highlight. An offset that isn't a grapheme boundary (which
    /// a well-behaved server should never send, but a byte offset from an
    /// untrusted source could still be malformed) snaps down to the
    /// enclosing grapheme's start rather than panicking.
    #[allow(dead_code)] // see the comment above `display_len`
    pub fn utf8_to_display(&self, utf8_offset: usize) -> usize {
        self.find_by_utf8(utf8_offset)
            .map_or(self.display_len, |seg| seg.display_start)
    }

    /// As [`Self::utf8_to_display`], but from a UTF-16 code-unit offset. An
    /// offset pointing at the low surrogate of an astral character (which,
    /// again, a spec-compliant server shouldn't send, but nothing stops a
    /// buggy one) snaps down to that character's start the same way a
    /// mid-grapheme display column does.
    #[allow(dead_code)] // see the comment above `display_len`
    pub fn utf16_to_display(&self, utf16_offset: usize) -> usize {
        self.find_by_utf16(utf16_offset)
            .map_or(self.display_len, |seg| seg.display_start)
    }

    fn find_by_display(&self, display_col: usize) -> Option<&Segment> {
        if display_col >= self.display_len {
            return None;
        }
        self.segments
            .iter()
            .find(|seg| display_col < seg.display_start + seg.display_width)
    }

    fn find_by_utf8(&self, utf8_offset: usize) -> Option<&Segment> {
        if utf8_offset >= self.utf8_len {
            return None;
        }
        self.segments
            .iter()
            .find(|seg| utf8_offset < seg.utf8_start + seg.utf8_len)
    }

    fn find_by_utf16(&self, utf16_offset: usize) -> Option<&Segment> {
        if utf16_offset >= self.utf16_len {
            return None;
        }
        self.segments
            .iter()
            .find(|seg| utf16_offset < seg.utf16_start + seg.utf16_len)
    }
}

/// The file and 0-based line an LSP request should target for `row`, or
/// `None` when there's nothing meaningful to ask a server about: a `Del`
/// row's text only ever existed on the old side, so it has no `new_line`
/// and no position in the file the server has open. Rows on a deleted or
/// binary file are excluded the same way — a `DiffFile` for a path git
/// deleted has no current content to look up.
///
/// The returned path is repo-root-relative, exactly as it appears in the
/// diff — this function has no notion of a repository root or a working
/// directory, so it never touches the filesystem to confirm the file is
/// really there. That's deliberate: resolving the path to an absolute one
/// (joining it against the repo root) and confirming it opens are both the
/// caller's job, once it has that context. A missing or unreadable file
/// then surfaces the same way any other hover failure does — no result, a
/// status-bar message — rather than this function needing an I/O-capable
/// signature just to special-case it earlier.
pub fn lsp_target(row: &DiffRow, file: &DiffFile) -> Option<(PathBuf, u32)> {
    if row.kind == DiffLineKind::Del {
        return None;
    }
    if file.is_deleted || file.is_binary {
        return None;
    }
    let new_line = row.new_line?;
    let path = file.new_path.as_deref()?;
    Some((PathBuf::from(path), new_line.saturating_sub(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_line_maps_one_to_one_across_all_three_systems() {
        let map = ColumnMap::new("hello");
        assert_eq!(map.display_len(), 5);
        assert_eq!(map.utf8_len(), 5);
        assert_eq!(map.utf16_len(), 5);
        for col in 0..5 {
            assert_eq!(map.display_to_utf8(col), col);
            assert_eq!(map.display_to_utf16(col), col);
            assert_eq!(map.utf8_to_display(col), col);
            assert_eq!(map.utf16_to_display(col), col);
        }
    }

    #[test]
    fn japanese_line_is_two_display_cells_per_character_but_one_utf16_unit() {
        // "こんにちは" — 5 characters, each a BMP codepoint (1 UTF-16 unit,
        // 3 UTF-8 bytes), each rendered 2 columns wide.
        let map = ColumnMap::new("こんにちは");
        assert_eq!(map.display_len(), 10);
        assert_eq!(map.utf8_len(), 15);
        assert_eq!(map.utf16_len(), 5);

        // The third character starts at display col 4, utf8 byte 6, utf16
        // unit 2.
        assert_eq!(map.display_to_utf8(4), 6);
        assert_eq!(map.display_to_utf16(4), 2);
        assert_eq!(map.utf8_to_display(6), 4);
        assert_eq!(map.utf16_to_display(2), 4);

        // Column 5 lands mid-character (between the two cells of the third
        // character) — snaps back to that character's start, not forward to
        // the next one.
        assert_eq!(map.display_to_utf8(5), 6);
        assert_eq!(map.display_to_utf16(5), 2);
    }

    #[test]
    fn mixed_ascii_and_japanese_identifier_converts_at_every_boundary() {
        // Byte/col layout:
        //   l e t   空 白 (name)   =     " a b c " ;
        // ASCII "let " is cols/bytes/units 0..4 (identity mapping).
        // 名 starts at display 4 / utf8 4 / utf16 4, width 2, len 3 bytes.
        // 前 starts at display 6 / utf8 7 / utf16 5, width 2, len 3 bytes.
        // " = \"abc\";" starts at display 8 / utf8 10 / utf16 6.
        let line = "let 名前 = \"abc\";";
        let map = ColumnMap::new(line);

        assert_eq!(map.display_to_utf8(0), 0);
        assert_eq!(map.display_to_utf16(0), 0);

        assert_eq!(map.display_to_utf8(4), 4);
        assert_eq!(map.display_to_utf16(4), 4);

        assert_eq!(map.display_to_utf8(6), 7);
        assert_eq!(map.display_to_utf16(6), 5);

        assert_eq!(map.display_to_utf8(8), 10);
        assert_eq!(map.display_to_utf16(8), 6);

        // Round-trip the "=" sign's position back to a display column.
        let eq_utf8 = map.display_to_utf8(9);
        assert_eq!(map.utf8_to_display(eq_utf8), 9);
    }

    /// Empirical backing for this module's doc comment: `ui::text` never
    /// special-cases `\t`, so `unicode-width` 0.2's default width for it (1
    /// column, not an expansion to a 4-column stop) is what actually ends up
    /// on screen. `ColumnMap` must agree with that, not with a nicer rule
    /// the renderer doesn't implement.
    #[test]
    fn tab_is_one_display_column_like_diff_view_renders_it() {
        use crate::ui::text::display_width;
        assert_eq!(
            display_width("\t"),
            1,
            "ui::text must still not expand tabs for this test's premise to hold"
        );

        let map = ColumnMap::new("a\tb");
        assert_eq!(map.display_len(), 3);
        assert_eq!(map.display_to_utf8(1), 1); // the tab itself
        assert_eq!(map.display_to_utf8(2), 2); // 'b', immediately after
        assert_eq!(map.utf8_to_display(2), 2);
    }

    #[test]
    fn emoji_astral_plane_is_a_utf16_surrogate_pair() {
        // U+1F600 GRINNING FACE: outside the BMP, so UTF-16 represents it as
        // a 2-unit surrogate pair even though it's one grapheme cluster and
        // (per unicode-width's East Asian width rules) 2 display columns.
        let line = "😀x";
        let map = ColumnMap::new(line);
        assert_eq!(map.display_len(), 3); // 2 (emoji) + 1 ('x')
        assert_eq!(map.utf8_len(), 5); // 4 bytes (emoji) + 1 ('x')
        assert_eq!(map.utf16_len(), 3); // 2 units (surrogate pair) + 1

        // 'x' starts right after the emoji in every coordinate system.
        assert_eq!(map.display_to_utf8(2), 4);
        assert_eq!(map.display_to_utf16(2), 2);
        assert_eq!(map.utf8_to_display(4), 2);
        assert_eq!(map.utf16_to_display(2), 2);

        // A UTF-16 offset of 1 points at the emoji's low surrogate — not a
        // valid character boundary — and must snap back to the emoji's own
        // start (display column 0), not forward past it.
        assert_eq!(map.utf16_to_display(1), 0);
    }

    #[test]
    fn empty_line_has_zero_length_in_every_system_and_clamps_every_query_to_zero() {
        let map = ColumnMap::new("");
        assert_eq!(map.display_len(), 0);
        assert_eq!(map.utf8_len(), 0);
        assert_eq!(map.utf16_len(), 0);
        assert_eq!(map.display_to_utf8(0), 0);
        assert_eq!(map.display_to_utf8(50), 0);
        assert_eq!(map.utf8_to_display(0), 0);
        assert_eq!(map.utf16_to_display(0), 0);
    }

    #[test]
    fn position_past_end_of_line_clamps_to_the_line_end_in_every_direction() {
        let map = ColumnMap::new("abc");
        assert_eq!(map.display_to_utf8(3), 3); // exactly at end: valid, not "past"
        assert_eq!(map.display_to_utf8(100), 3); // genuinely past: clamps
        assert_eq!(map.display_to_utf16(100), 3);
        assert_eq!(map.utf8_to_display(100), 3);
        assert_eq!(map.utf16_to_display(100), 3);
    }

    fn row(kind: DiffLineKind, old_line: Option<u32>, new_line: Option<u32>) -> DiffRow {
        DiffRow {
            kind,
            text: "irrelevant".to_owned(),
            old_line,
            new_line,
        }
    }

    fn file(new_path: Option<&str>, is_deleted: bool, is_binary: bool) -> DiffFile {
        DiffFile {
            old_path: None,
            new_path: new_path.map(str::to_owned),
            hunks: Vec::new(),
            is_new: false,
            is_deleted,
            is_renamed: false,
            is_binary,
        }
    }

    #[test]
    fn lsp_target_resolves_context_and_add_rows_to_a_zero_based_line() {
        let f = file(Some("src/lib.rs"), false, false);
        let ctx = row(DiffLineKind::Context, Some(10), Some(12));
        assert_eq!(
            lsp_target(&ctx, &f),
            Some((PathBuf::from("src/lib.rs"), 11))
        );

        let add = row(DiffLineKind::Add, None, Some(1));
        assert_eq!(lsp_target(&add, &f), Some((PathBuf::from("src/lib.rs"), 0)));
    }

    #[test]
    fn lsp_target_refuses_del_rows_deleted_files_and_binary_files() {
        let f = file(Some("src/lib.rs"), false, false);
        let del = row(DiffLineKind::Del, Some(5), None);
        assert_eq!(
            lsp_target(&del, &f),
            None,
            "a del row has no new_line to target"
        );

        let deleted_file = file(Some("src/gone.rs"), true, false);
        let ctx_on_deleted = row(DiffLineKind::Context, Some(1), Some(1));
        assert_eq!(
            lsp_target(&ctx_on_deleted, &deleted_file),
            None,
            "a deleted file has nothing on disk for a server to open"
        );

        let binary_file = file(Some("assets/logo.png"), false, true);
        let add_on_binary = row(DiffLineKind::Add, None, Some(1));
        assert_eq!(lsp_target(&add_on_binary, &binary_file), None);
    }
}
