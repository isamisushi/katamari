//! CJK regression suite (M7): a dedicated home for width/coordinate tests
//! that exercise the *combination* of modules a Japanese-content diff
//! actually flows through end to end — parsing, flattening, column
//! conversion, span truncation, comment anchoring, symbol scanning, and the
//! keymap resolver — rather than any one module's isolated unit tests
//! (which already cover each piece on its own; see `diff::coords`,
//! `ui::text`, `ui::symbols`, and `comments`'s own test modules). East
//! Asian wide characters are where display-column math most easily drifts
//! from byte/UTF-16 math, so this module exists to catch a regression that
//! only shows up once several of those pieces are wired together — a
//! purpose distinct enough from any single module's tests to warrant its
//! own file rather than growing one of theirs.
//!
//! Test-only: this module has no non-test content, so it's declared behind
//! `#[cfg(test)]` in `main.rs` rather than compiled into the binary.

#[cfg(test)]
mod tests {
    use crate::comments::{self, Comment, Status};
    use crate::diff::{ColumnMap, RenderRow, flatten, flatten_side_by_side, parse_unified_diff};
    use crate::highlight::{Language, LineHighlighter};
    use crate::keymap::{KeyChord, Keymap, StepResult, vim_preset};
    use crate::ui::symbols;
    use crate::ui::text::{display_width, truncate_spans_to_width, truncate_to_width};
    use crossterm::event::{KeyCode, KeyModifiers};

    const JAPANESE_FIXTURE: &str = include_str!("diff/fixtures/japanese.diff");

    /// `git diff` -> `parse_unified_diff` -> `flatten` -> `ColumnMap` on the
    /// resulting row text, round-tripped display -> utf8 -> display and
    /// display -> utf16 -> display, for every content line in a real
    /// (fixture) Japanese diff — not just a hand-picked string literal.
    /// Guards the full parse-to-coordinate pipeline, not just `ColumnMap`
    /// in isolation.
    #[test]
    fn parsed_japanese_diff_rows_round_trip_through_column_map_in_both_encodings() {
        let files = parse_unified_diff(JAPANESE_FIXTURE);
        let rows = flatten(&files);
        let mut checked_a_wide_line = false;

        for row in rows {
            let RenderRow::Line {
                file_idx,
                hunk_idx,
                row_idx,
            } = row
            else {
                continue;
            };
            let text = &files[file_idx].hunks[hunk_idx].rows[row_idx].text;
            if text.is_empty() {
                continue;
            }
            let columns = ColumnMap::new(text);
            if display_width(text) > text.chars().count() {
                checked_a_wide_line = true;
            }
            for col in 0..columns.display_len() {
                let utf8 = columns.display_to_utf8(col);
                let utf16 = columns.display_to_utf16(col);
                // Both round trips must land back on a display column
                // that's at or before `col` (never past it — a
                // mid-character column always snaps to its character's
                // start, so the round trip is idempotent from there).
                assert!(columns.utf8_to_display(utf8) <= col);
                assert!(columns.utf16_to_display(utf16) <= col);
            }
        }
        assert!(
            checked_a_wide_line,
            "the fixture must actually contain a wide-character line for this test to mean anything"
        );
    }

    /// `println!("こんにちは、世界");` from the fixture: every ASCII run
    /// converts 1:1 across all three coordinate systems, and every wide
    /// Japanese character is exactly 2 display columns / 3 UTF-8 bytes / 1
    /// UTF-16 unit (all five characters are BMP codepoints) — spelled out
    /// explicitly (not just "round trips"), since a bug that shifted every
    /// column by a constant offset would still "round trip" successfully.
    #[test]
    fn a_real_diff_line_mixing_ascii_and_japanese_has_the_expected_absolute_widths() {
        let line = r#"    println!("こんにちは、世界");"#;
        let columns = ColumnMap::new(line);
        // 4 spaces + `println!("` = 15 ASCII bytes/columns before 「こ」.
        let prefix = r#"    println!(""#;
        assert_eq!(display_width(prefix), prefix.chars().count());
        let japanese_start_utf8 = prefix.len();
        let japanese_start_display = prefix.chars().count();

        assert_eq!(
            columns.utf8_to_display(japanese_start_utf8),
            japanese_start_display
        );
        // 5 wide characters (こんにちは) each occupy 2 display columns.
        assert_eq!(
            columns.display_to_utf8(japanese_start_display + 2),
            japanese_start_utf8 + 3
        );
        assert_eq!(
            columns.display_to_utf16(japanese_start_display + 2),
            japanese_start_display + 1
        );
    }

    /// Highlighting a Japanese line, then truncating at a display width
    /// that lands mid-character, must never split a wide character's bytes
    /// across the cut — the span's text stays valid UTF-8 covering only
    /// whole characters. Exercises `LineHighlighter` + `truncate_spans_to_width`
    /// together, the actual pipeline `ui::diff_view::content_line` runs,
    /// rather than `truncate_spans_to_width` alone (already covered in
    /// `ui::text`'s own tests with hand-built spans).
    #[test]
    fn truncating_highlighted_japanese_spans_never_splits_a_wide_character() {
        let mut highlighter = LineHighlighter::new();
        let line = "// 日本語のコメントを追加する";
        let spans = highlighter.highlight_line(Language::Rust, line);

        // Budget of 5 columns: "// " (3) + one 2-wide character (5) — the
        // next character would need column 7, so it must NOT appear.
        let truncated = truncate_spans_to_width(&spans, 5);
        let joined: String = truncated.iter().map(|s| s.text.as_str()).collect();
        assert!(display_width(&joined) <= 5);
        // Every character in the truncated text must also appear as a
        // contiguous prefix of the original line — proof no character was
        // cut mid-byte-sequence (which would produce invalid UTF-8 and fail
        // to construct as a `String` at all, but this also checks it wasn't
        // silently replaced or reordered).
        assert!(line.starts_with(&joined));
    }

    /// `truncate_to_width` directly against the fixture's comment line, at
    /// a handful of widths straddling every character boundary — the
    /// boundary-splitting case the milestone spec calls out specifically,
    /// run across a real line rather than a synthetic one.
    #[test]
    fn truncate_to_width_never_exceeds_budget_at_any_boundary_in_a_real_japanese_line() {
        let line = "日本語のコメントを追加する";
        for width in 0..=display_width(line) + 2 {
            let truncated = truncate_to_width(line, width);
            assert!(display_width(&truncated) <= width);
            assert!(line.starts_with(&truncated));
        }
    }

    /// `comments::anchor_for`/`relocate` on a file whose anchored line is
    /// Japanese: the content hash must match on the unchanged line, and
    /// `relocate` must follow it after an unrelated insertion shifts its
    /// line number — the CJK case of the anchor-drift tests `comments::mod`
    /// already runs on ASCII content.
    #[test]
    fn comment_anchored_to_a_japanese_line_relocates_after_an_insertion_above_it() {
        let before = ["fn main() {", "    println!(\"こんにちは、世界\");", "}"];
        let anchor = comments::anchor_for(&before, 2).expect("line 2 exists");
        let comment = Comment {
            id: "cjk1".to_owned(),
            created_at: 0,
            file: "src/main.rs".to_owned(),
            anchor,
            body: "この行にコメント".to_owned(),
            status: Status::Open,
            resolved_at: None,
        };

        // Unchanged: relocates to the same line.
        assert_eq!(comments::relocate(&comment, &before), Some(2));

        // An unrelated line inserted above it shifts the anchor down by
        // one; the Japanese line's content hash still matches.
        let after = [
            "fn main() {",
            "    // 日本語のコメントを追加する",
            "    println!(\"こんにちは、世界\");",
            "}",
        ];
        assert_eq!(comments::relocate(&comment, &after), Some(3));
    }

    /// The anchored Japanese line itself gets deleted — `relocate` must
    /// report `None` (detached), not silently latch onto some unrelated
    /// line, and not panic on the multi-byte content it's hashing.
    #[test]
    fn comment_anchored_to_a_japanese_line_detaches_when_that_line_is_deleted() {
        let before = ["fn main() {", "    println!(\"こんにちは、世界\");", "}"];
        let anchor = comments::anchor_for(&before, 2).expect("line 2 exists");
        let comment = Comment {
            id: "cjk2".to_owned(),
            created_at: 0,
            file: "src/main.rs".to_owned(),
            anchor,
            body: "comment".to_owned(),
            status: Status::Open,
            resolved_at: None,
        };

        let after = ["fn main() {", "}"];
        assert_eq!(comments::relocate(&comment, &after), None);
    }

    /// `ui::symbols::scan`'s display-column output for a Japanese
    /// identifier run must land at the same columns `ColumnMap` would
    /// convert to/from for that same text — the coordinate-space agreement
    /// `Action::Hover`'s request-building depends on (see
    /// `App::hover_query`, which feeds `symbols::scan`'s output straight
    /// into `ColumnMap`-based LSP position conversion).
    #[test]
    fn symbol_scan_display_columns_for_a_japanese_identifier_agree_with_column_map() {
        let line = "let 名前 = 1;";
        let syms = symbols::scan(line);
        // "名前" is the second symbol ("let" is the first).
        let name_symbol = syms[1];

        let columns = ColumnMap::new(line);
        // The symbol's display_start must convert to the exact UTF-8 byte
        // offset where "名" begins: "let " is 4 ASCII bytes/columns.
        assert_eq!(name_symbol.display_start, 4);
        assert_eq!(columns.display_to_utf8(name_symbol.display_start), 4);
        // And round back to the same display column.
        assert_eq!(
            columns.utf8_to_display(columns.display_to_utf8(name_symbol.display_start)),
            name_symbol.display_start
        );
    }

    /// `flatten_side_by_side` on the Japanese fixture doesn't lose or
    /// reorder any content row relative to `flatten`'s own order — the
    /// side-by-side layout pairs rows up differently, but every
    /// `RenderRow::Line` it addresses (by flat index) must still resolve to
    /// the exact same Japanese text `flatten` produced.
    #[test]
    fn side_by_side_pairing_preserves_every_japanese_content_row() {
        use crate::diff::{SideBySideRow, SideCell};

        let files = parse_unified_diff(JAPANESE_FIXTURE);
        let rows = flatten(&files);
        let paired = flatten_side_by_side(&files);

        let flat_indices_in_pairing: Vec<usize> = paired
            .into_iter()
            .flat_map(|entry| match entry {
                SideBySideRow::Full { flat_idx } => vec![flat_idx],
                SideBySideRow::Paired { old, new } => [old, new]
                    .into_iter()
                    .filter_map(|cell| match cell {
                        SideCell::Line { flat_idx } => Some(flat_idx),
                        SideCell::Empty => None,
                    })
                    .collect(),
            })
            .collect();

        let row_text = |flat_idx: usize| -> Option<&str> {
            let RenderRow::Line {
                file_idx,
                hunk_idx,
                row_idx,
            } = rows[flat_idx]
            else {
                return None;
            };
            Some(files[file_idx].hunks[hunk_idx].rows[row_idx].text.as_str())
        };

        let seen_japanese_row = flat_indices_in_pairing
            .iter()
            .filter_map(|&idx| row_text(idx))
            .any(|text| !text.is_ascii());
        assert!(
            seen_japanese_row,
            "the side-by-side pairing must still surface at least one Japanese content row"
        );
    }

    /// A raw CJK character arriving as a key press (crossterm reports a
    /// full Unicode scalar in `KeyCode::Char`, exactly as it would for
    /// composed IME input or a literal keypress on a CJK-labeled key) must
    /// not panic the resolver — it's simply not bound to anything in the
    /// vim preset, so it cancels any pending sequence like any other
    /// unbound key would. Keymap resolution being oblivious to *which*
    /// Unicode character arrives (not just ASCII ones) is the property this
    /// pins down.
    #[test]
    fn keymap_resolver_treats_an_unbound_cjk_character_as_an_ordinary_cancelled_key() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let mut resolver = keymap.resolver();
        let cjk_key = KeyChord::new(KeyCode::Char('日'), KeyModifiers::NONE);
        assert_eq!(resolver.feed(cjk_key), StepResult::Cancelled);

        // The resolver is still usable afterward — an ordinary vim binding
        // resolves normally right after the unbound CJK key.
        let q = KeyChord::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(
            resolver.feed(q),
            StepResult::Matched(crate::keymap::Action::Quit)
        );
    }

    /// M13: a line long enough to soft-wrap, with a CJK comment prefix
    /// sitting before the identifier under the cursor —
    /// `App::hover_query`'s display-column pipeline
    /// (`ui::symbols::scan` + eventual `ColumnMap` conversion) reads only a
    /// row's raw, un-wrapped text and never consults pane width or
    /// `App::content_width` at all, so wrapping a line for display must
    /// never perturb the display column (and therefore the UTF-8/UTF-16
    /// offset a real LSP request would carry) it reports for an identifier
    /// that ends up on a wrapped continuation row — the cursor→LSP position
    /// path this milestone's spec calls out as needing a regression test.
    #[test]
    fn hover_query_display_column_is_unaffected_by_wrapping_a_line_with_a_cjk_prefix() {
        use crate::diff::{DiffFile, DiffHunk, DiffLineKind, DiffRow};
        use crate::keymap::Action;
        use crate::ui::app::App;
        use std::path::PathBuf;

        // "日本語のコメントです" is 10 characters, 20 display columns —
        // combined with "xxx...x" padding this line comfortably exceeds
        // 100 display columns, wrapping into several visual rows at any
        // pane narrower than that.
        let cjk_prefix = "日本語のコメントです";
        let padding = "x".repeat(90);
        let line = format!("// {cjk_prefix} {padding} target_symbol();");

        let file = DiffFile {
            old_path: Some("f.rs".to_owned()),
            new_path: Some("f.rs".to_owned()),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                header: String::new(),
                // Not testing fold rows here — pin this fixture to the
                // pre-fold-feature shape (no trailing gap) so this test
                // stays about wrapping/hover, not gap arithmetic.
                known_eof: true,
                rows: vec![DiffRow {
                    kind: DiffLineKind::Context,
                    text: line.clone(),
                    old_line: Some(1),
                    new_line: Some(1),
                }],
            }],
            ..Default::default()
        };
        let mut app = App::new("repo".to_owned(), PathBuf::from("/repo"), vec![file]);
        app.set_viewport_height(10);
        app.update(Action::Top);
        app.update(Action::CursorDown); // hunk header
        app.update(Action::CursorDown); // the content row
        assert_eq!(app.cursor, 2, "sanity: cursor is on the content row");

        // Symbols on this line, in order: the CJK comment run, the 90-`x`
        // padding run, then `target_symbol` — the third.
        app.active_symbol = 2;

        // Unwrapped (an effectively unbounded pane) and wrapped (a pane
        // narrow enough that this line splits across several continuation
        // rows) must report the exact same `HoverQuery` — wrapping is pure
        // presentation, never a second source of truth for where the
        // cursor's identifier sits.
        app.set_content_width(usize::MAX);
        let unwrapped = app.hover_query().expect("target_symbol is hover-eligible");
        app.set_content_width(20); // forces the line to wrap several times over
        let wrapped = app
            .hover_query()
            .expect("still hover-eligible once wrapped");

        assert_eq!(unwrapped, wrapped);
        assert_eq!(wrapped.line_text, line);
        assert_eq!(wrapped.line, 0); // new_line 1, 0-based
        // "// " (3) + the CJK run (20) + " " (1) + 90 `x`s + " " (1).
        assert_eq!(wrapped.display_col, 3 + 20 + 1 + 90 + 1);
    }

    /// A tab followed by wide Japanese characters — the M7.3 tab-stop work
    /// (`ColumnMap`'s tab-aware width rule) and CJK wide-character width
    /// must compose correctly, not just each work in isolation: the tab
    /// advances to the configured stop, and the Japanese characters after
    /// it are still exactly 2 columns each from there.
    #[test]
    fn a_tab_before_japanese_text_advances_to_the_stop_before_wide_characters_begin() {
        let line = "\t名前"; // tab width 4: tab -> column 4, then "名前" (2 wide chars)
        let columns = ColumnMap::with_tab_width(line, 4);
        assert_eq!(columns.display_len(), 8); // 4 (tab) + 2 + 2
        // "名" starts right after the tab, at display column 4.
        assert_eq!(columns.display_to_utf8(4), 1); // 1 byte for the tab itself
        // "前" starts at display column 6.
        assert_eq!(columns.utf8_to_display(1 + "名".len()), 6);
    }
}
