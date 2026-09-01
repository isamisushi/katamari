//! Renders the changed-file list as a collapsible directory tree (issue
//! #15): one line per [`crate::ui::file_tree::VisibleRow`] — a directory's
//! disclosure glyph and descendant-file count, or a file's status badge and
//! `+/-` stat counts. Issue #14 gave this pane two independent things to
//! mark, not one: [`App::files_selection`] (the sidebar's own browsed
//! position, reversed/bold) and [`App::diff_file_visible_row`] (whichever
//! row the background diff cursor's file currently resolves to, cyan/
//! underlined when it differs from the selection) — see those fields' docs
//! for why they're allowed to diverge at all. Renders through [`PaneChrome`]
//! for the same focused-border/bottom-hint treatment every other focusable
//! pane gets.

use crate::diff::DiffFile;
use crate::keymap::Keymap;
use crate::ui::app::{App, MainPaneFocus};
use crate::ui::file_tree::{VisibleKind, VisibleRow};
use crate::ui::hints;
use crate::ui::pane::PaneChrome;
use crate::ui::text::{display_width, truncate_to_width};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Columns of indentation per tree depth level.
const INDENT_WIDTH: usize = 2;
/// The disclosure-glyph/badge column plus its trailing space — one column
/// wide so `▾`/`▸`/`A`/`D`/`R`/`M` all land in the same place regardless of
/// row kind, keeping the tree's marker column aligned top to bottom.
const MARKER_WIDTH: usize = 2;
/// The narrowest a label is ever allowed to shrink to before indentation
/// itself gets capped (req 11) — an extremely deep path in a narrow pane
/// must still show *some* of its own name, not just indentation and a
/// marker.
const MIN_LABEL_WIDTH: usize = 4;

pub fn render(frame: &mut Frame, area: Rect, app: &App, keymap: &Keymap) {
    let focused = app.focus == MainPaneFocus::Files;
    let block = PaneChrome::new(" files ", area.width)
        .focused(focused)
        .hints(&hints::files_pane_hints(keymap))
        .block();
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let diff_row = app.diff_file_visible_row();
    let width = inner.width as usize;
    let viewport = inner.height as usize;

    let lines: Vec<Line> = app
        .visible_rows
        .iter()
        .enumerate()
        .skip(app.files_scroll_offset)
        .take(viewport)
        .map(|(idx, row)| {
            let mut line = render_row(row, &app.files, &app.reviewed_by_file, width);
            if idx == app.files_selection {
                // Reversed while focused (the classic "this is where your
                // cursor is" treatment); bold-only while `Diff` owns focus,
                // so the selection still reads as "the sidebar's position"
                // without looking like an active cursor it isn't right now.
                let modifier = if focused {
                    Modifier::REVERSED
                } else {
                    Modifier::BOLD
                };
                line = line.patch_style(Style::default().add_modifier(modifier));
            } else if Some(idx) == diff_row {
                line = line.patch_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            line
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

/// One [`VisibleRow`] rendered into a `width`-column-wide [`Line`]:
/// indentation (capped per [`MIN_LABEL_WIDTH`]'s docs), the marker column
/// (disclosure glyph or status badge), the label — truncated by display
/// width (req 11: CJK/deep paths never split a grapheme or blow past
/// `width`) — and, when it fits *without* truncating the label, a droppable
/// suffix (a file's `+A -D` stat plus, when it has any reviewed hunks, a
/// trailing `n/m✓` count, or a directory's `(N)` descendant count).
/// `reviewed_by_file` is [`App::reviewed_by_file`] — threaded through
/// rather than read off `app` directly so this stays a plain function of
/// its inputs, the same reason `files` already is.
fn render_row(
    row: &VisibleRow,
    files: &[DiffFile],
    reviewed_by_file: &[(usize, usize)],
    width: usize,
) -> Line<'static> {
    let capped_indent =
        (row.depth * INDENT_WIDTH).min(width.saturating_sub(MARKER_WIDTH + MIN_LABEL_WIDTH));
    let available = width.saturating_sub(capped_indent + MARKER_WIDTH);

    let (marker, marker_style) = marker_for(row, files);
    let suffix = suffix_for(row, files, reviewed_by_file);

    let full_label_width = display_width(&row.label);
    let (label, suffix_spans) = match &suffix {
        Some((suffix_text, spans))
            if full_label_width + 1 + display_width(suffix_text) <= available =>
        {
            (row.label.clone(), Some(spans))
        }
        _ => (truncate_to_width(&row.label, available), None),
    };

    let mut spans = vec![
        Span::raw(" ".repeat(capped_indent)),
        Span::styled(format!("{marker} "), marker_style),
        Span::raw(label),
    ];
    if let Some(suffix_spans) = suffix_spans {
        spans.push(Span::raw(" "));
        spans.extend(suffix_spans.iter().cloned());
    }
    Line::from(spans)
}

/// The marker column's glyph and style: `▾`/`▸` for a directory (expanded/
/// collapsed — req 5), or [`DiffFile::badge`]'s single letter for a file,
/// unstyled either way — the marker's *position*, not its color, is what
/// req 5 asks for, and an uncolored badge keeps the tree's own visual noise
/// down next to the selection/background-file highlighting `render` already
/// applies on top.
fn marker_for(row: &VisibleRow, files: &[DiffFile]) -> (String, Style) {
    match &row.kind {
        VisibleKind::Directory { expanded, .. } => {
            let glyph = if *expanded { "\u{25be}" } else { "\u{25b8}" };
            (glyph.to_owned(), Style::default())
        }
        VisibleKind::File { file_idx } => (files[*file_idx].badge().to_string(), Style::default()),
    }
}

/// The droppable suffix text (for the fit check) and its styled spans (for
/// rendering) — `None` is never returned; every row kind has one, unlike
/// the marker, which is always shown regardless of fit.
fn suffix_for(
    row: &VisibleRow,
    files: &[DiffFile],
    reviewed_by_file: &[(usize, usize)],
) -> Option<(String, Vec<Span<'static>>)> {
    match &row.kind {
        VisibleKind::Directory {
            descendant_files, ..
        } => {
            let text = format!("({descendant_files})");
            let spans = vec![Span::styled(
                text.clone(),
                Style::default().fg(Color::DarkGray),
            )];
            Some((text, spans))
        }
        VisibleKind::File { file_idx } => {
            let (added, deleted) = files[*file_idx].stat();
            let mut text = format!("+{added} -{deleted}");
            let mut spans = vec![
                Span::styled(format!("+{added} "), Style::default().fg(Color::Green)),
                Span::styled(format!("-{deleted}"), Style::default().fg(Color::Red)),
            ];
            // Only when the file has hunks at all worth a reviewed count —
            // a binary file's `reviewed_by_file` entry is always `(0, 0)`
            // (see `App::rederive`'s `enumerate_hunks` pass, which never
            // enumerates a binary file's absent hunks), so this stays out
            // of the way for exactly the files that already show no `z o`
            // fold affordance either.
            if let Some(&(reviewed, total)) = reviewed_by_file.get(*file_idx)
                && total > 0
            {
                let mark = if reviewed == total { "\u{2713}" } else { "" };
                let suffix = format!(" \u{b7} {reviewed}/{total}{mark}");
                text.push_str(&suffix);
                spans.push(Span::styled(
                    suffix,
                    Style::default()
                        .fg(if reviewed == total {
                            Color::Green
                        } else {
                            Color::DarkGray
                        })
                        .add_modifier(Modifier::DIM),
                ));
            }
            Some((text, spans))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::vim_preset;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn app_with_files(paths: &[&str]) -> App {
        let files: Vec<DiffFile> = paths
            .iter()
            .map(|p| DiffFile {
                old_path: Some((*p).to_owned()),
                new_path: Some((*p).to_owned()),
                ..Default::default()
            })
            .collect();
        App::new("repo".to_owned(), PathBuf::from("/repo"), files)
    }

    fn draw(app: &App, keymap: &Keymap, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), app, keymap))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        buffer.content.iter().map(|cell| cell.symbol()).collect()
    }

    #[test]
    fn focused_border_is_cyan_and_bold() {
        let mut app = app_with_files(&["a.rs", "b.rs"]);
        app.focus = MainPaneFocus::Files;
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let buffer = draw(&app, &keymap, 30, 8);
        let corner = buffer.cell((0, 0)).expect("top-left corner");
        assert_eq!(corner.fg, Color::Cyan);
        assert!(corner.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn unfocused_border_is_not_cyan() {
        let app = app_with_files(&["a.rs", "b.rs"]);
        assert_eq!(app.focus, MainPaneFocus::Diff);
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let buffer = draw(&app, &keymap, 30, 8);
        let corner = buffer.cell((0, 0)).expect("top-left corner");
        assert_ne!(corner.fg, Color::Cyan);
    }

    #[test]
    fn selection_and_background_file_get_visibly_distinct_styles_when_they_differ() {
        // Two-file diff: the diff cursor sits in file 0 (`App::diff_file`'s
        // default), but the reviewer has browsed the sidebar down to file 1
        // — the exact divergence req 5 requires stay visibly distinct.
        let mut app = app_with_files(&["a.rs", "b.rs"]);
        app.files_selection = 1;
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let buffer = draw(&app, &keymap, 30, 8);

        // Row 1 (inside the border) is "a.rs" — the background diff file,
        // marked cyan/underlined, not reversed.
        let a_cell = buffer.cell((1, 1)).expect("a.rs row, first column");
        assert_eq!(a_cell.fg, Color::Cyan);
        assert!(a_cell.modifier.contains(Modifier::UNDERLINED));
        assert!(!a_cell.modifier.contains(Modifier::REVERSED));

        // Row 2 is "b.rs" — the files-pane selection, bold (not focused).
        let b_cell = buffer.cell((1, 2)).expect("b.rs row, first column");
        assert!(b_cell.modifier.contains(Modifier::BOLD));
        assert!(!b_cell.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn scroll_offset_windows_the_visible_files() {
        // More files than the pane's inner height (8 rows - 2 borders - 1
        // bottom-hint row = 5 visible) with the selection scrolled well
        // past the top — the pre-#14 renderer drew the *whole* list with no
        // windowing at all (a latent bug this fixes), so file 0 must not
        // appear on screen once scrolled past.
        let paths: Vec<String> = (0..10).map(|i| format!("f{i}.rs")).collect();
        let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let mut app = app_with_files(&path_refs);
        app.files_selection = 9;
        app.files_scroll_offset = 9;
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let buffer = draw(&app, &keymap, 30, 8);
        let contents = buffer_text(&buffer);
        assert!(
            !contents.contains("f0.rs"),
            "scrolled-past file 0 must not render"
        );
        assert!(
            contents.contains("f9.rs"),
            "the scrolled-to file must render"
        );
    }

    // ---- tree rendering (issue #15) --------------------------------------

    #[test]
    fn ascii_columns_show_directory_glyphs_badges_and_indentation() {
        let app = app_with_files(&["src/lib.rs", "README.md"]);
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let buffer = draw(&app, &keymap, 40, 8);
        let contents = buffer_text(&buffer);
        assert!(contents.contains('\u{25be}'), "src starts expanded");
        assert!(contents.contains("src"));
        // The badge is asserted as the joined "M <label>" cell run — badge
        // in the marker column, one space, then the label — not a bare
        // contains('M'), which "README.md"'s own capital M would satisfy
        // even with the badge column entirely broken.
        assert!(contents.contains("M lib.rs"), "{contents}");
        assert!(contents.contains("M README.md"), "{contents}");
    }

    #[test]
    fn collapsed_directory_shows_the_collapsed_glyph_and_hides_its_child() {
        let mut app = app_with_files(&["src/lib.rs"]);
        app.focus = MainPaneFocus::Files;
        app.files_selection = 0; // the "src" directory row
        app.update(crate::keymap::Action::ToggleDirectory);
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let buffer = draw(&app, &keymap, 40, 8);
        let contents = buffer_text(&buffer);
        assert!(contents.contains('\u{25b8}'), "collapsed glyph present");
        assert!(!contents.contains("lib.rs"), "child row must be hidden");
    }

    #[test]
    fn directory_suffix_shows_descendant_file_count() {
        let app = app_with_files(&["src/a.rs", "src/b.rs"]);
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let buffer = draw(&app, &keymap, 40, 8);
        let contents = buffer_text(&buffer);
        assert!(contents.contains("(2)"), "src has two changed descendants");
    }

    fn app_with_a_hunk(path: &str) -> App {
        use crate::diff::{DiffHunk, DiffLineKind, DiffRow};
        let file = DiffFile {
            old_path: Some(path.to_owned()),
            new_path: Some(path.to_owned()),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
                header: String::new(),
                known_eof: false,
                rows: vec![DiffRow {
                    kind: DiffLineKind::Add,
                    text: "x".to_owned(),
                    old_line: None,
                    new_line: Some(1),
                }],
            }],
            ..Default::default()
        };
        App::new("repo".to_owned(), PathBuf::from("/repo"), vec![file])
    }

    #[test]
    fn suffix_for_a_file_with_no_hunks_omits_the_reviewed_count() {
        let files = vec![DiffFile {
            old_path: Some("a.rs".to_owned()),
            new_path: Some("a.rs".to_owned()),
            ..Default::default()
        }];
        let row = VisibleRow {
            id: crate::ui::file_tree::NodeId {
                path: "a.rs".to_owned(),
                is_directory: false,
            },
            depth: 0,
            label: "a.rs".to_owned(),
            kind: VisibleKind::File { file_idx: 0 },
        };
        let (text, _) = suffix_for(&row, &files, &[(0, 0)]).unwrap();
        assert_eq!(text, "+0 -0", "no hunks — no reviewed count appended");
    }

    #[test]
    fn per_file_suffix_shows_the_reviewed_count_and_a_checkmark_once_fully_reviewed() {
        let mut app = app_with_a_hunk("a.rs");
        let keymap = Keymap::from_bindings(&vim_preset(false));

        let before = buffer_text(&draw(&app, &keymap, 30, 8));
        assert!(
            before.contains("0/1") && !before.contains("\u{2713}"),
            "unreviewed: count shows but no checkmark yet:\n{before}"
        );

        app.mark_file_reviewed();
        let after = buffer_text(&draw(&app, &keymap, 30, 8));
        assert!(
            after.contains("1/1") && after.contains("\u{2713}"),
            "fully reviewed: count and checkmark both show:\n{after}"
        );
    }

    /// req 11: a CJK (double-width) label narrower than the pane still
    /// renders in full; one wider than the available width still renders
    /// without panicking (grapheme-safe truncation, proven by
    /// `truncate_to_width`'s own unit tests — this only pins down that
    /// [`render_row`] actually reaches that code path at a real narrow
    /// width rather than, say, byte-slicing and producing invalid UTF-8).
    #[test]
    fn cjk_label_renders_without_panicking_at_a_narrow_width() {
        let app = app_with_files(&["日本語のとても長いファイル名です.txt"]);
        let keymap = Keymap::from_bindings(&vim_preset(false));
        // Narrow enough that the CJK (double-width) label can't fit whole
        // alongside the badge/indent columns.
        let buffer = draw(&app, &keymap, 16, 8);
        let contents = buffer_text(&buffer);
        // The badge column ('M') stays intact even though the label had to
        // truncate.
        assert!(contents.contains('M'));
    }

    #[test]
    fn a_very_deep_path_caps_indentation_and_still_shows_part_of_its_own_label() {
        // 10 nested single-letter directories (a..j) plus the file leaf —
        // 11 rows total; a tall enough pane (15 - 2 border = 13 inner rows)
        // to keep every one of them on screen at once.
        let app = app_with_files(&["a/b/c/d/e/f/g/h/i/j/deep.rs"]);
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let buffer = draw(&app, &keymap, 20, 15);
        let contents = buffer_text(&buffer);
        // At depth 10 with a 20-column pane, indentation is capped well
        // short of `depth * INDENT_WIDTH` — leaving just enough width for
        // `deep.rs` to truncate to "deep" rather than disappearing under
        // pure indentation.
        assert!(
            contents.contains("deep"),
            "deep path's own label must remain partially visible, capped indent notwithstanding"
        );
    }
}
