//! OSC 52 transport and diff-selection formatting for issue #17's `y`
//! (yank), shared with the LSP inspector's own pre-existing Journal copy
//! (issue predates this module — see [`write_osc52`]'s docs). Splitting
//! this out of `ui::lsp_inspector` does two things: it removes the
//! double-maintained OSC 52/base64 code that module and the main diff view
//! would otherwise each need their own copy of, and it isolates the pure
//! formatting half ([`resolve_selection`]/[`format_diff_selection`]) from
//! any I/O, so the format the issue specifies is covered by ordinary unit
//! tests instead of needing a terminal.

use crate::diff::{DiffFile, DiffLineKind, RenderRow};
use std::collections::HashSet;
use std::io::{self, Write};

/// OSC 52 is sent through a terminal escape sequence, so refusing oversized
/// selections before encoding them keeps a keypress from flooding a
/// terminal/multiplexer and makes the copy operation's privacy boundary
/// explicit.
pub const OSC52_MAX_BYTES: usize = 64 * 1024;

/// Emits a terminal-native clipboard update. OSC 52 avoids invoking a
/// platform-specific clipboard process and can work over SSH or a configured
/// multiplexer; terminal support and policy determine whether the sequence is
/// actually accepted. The caller has already applied the byte bound and its
/// own selected-content mapping.
///
/// Hand-rolled on purpose: crossterm 0.29 ships a byte-identical
/// `clipboard::CopyToClipboard`, but only behind its non-default `osc52`
/// feature, which drags in the `base64` crate — a new dependency to replace
/// forty tested lines isn't a trade this repo makes.
pub fn write_osc52(text: &str) -> io::Result<()> {
    if text.len() > OSC52_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "OSC 52 payload exceeds the inspector copy limit",
        ));
    }
    let encoded = base64_encode(text.as_bytes());
    let mut stdout = io::stdout().lock();
    write!(stdout, "\x1b]52;c;{encoded}\x1b\\")?;
    stdout.flush()
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        let second = chunk.get(1).copied();
        encoded.push(
            ALPHABET[((first & 0x03) << 4 | second.map_or(0, |byte| byte >> 4)) as usize] as char,
        );
        if let Some(second) = second {
            encoded.push(
                ALPHABET[((second & 0x0f) << 2 | chunk.get(2).copied().map_or(0, |byte| byte >> 6))
                    as usize] as char,
            );
        } else {
            encoded.push('=');
        }
        if let Some(third) = chunk.get(2).copied() {
            encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

/// One selected diff line's paste-ready fields, resolved from a flat
/// `RenderRow` index back to the underlying content. Not `DiffRow` itself:
/// a `DiffRow` has no path, and nothing downstream of resolution needs the
/// file/hunk/row indices once the actual content is in hand.
pub struct SelectedLine<'a> {
    pub path: &'a str,
    pub kind: DiffLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: &'a str,
}

/// Maps each selected flat-row index (typically `App::selected_rows()`, in
/// screen order) back to the `DiffRow` it renders. Defensive against a
/// stale or out-of-range index rather than panicking — mirrors the
/// inspector's own bounds-checked `copy_selection` — because a keypress
/// should never crash even if some future refactor lets `rows`/`selected`
/// drift out of sync. Also the only place structural rows actually get
/// filtered out on this path: `App::selected_rows()` already excludes
/// them, but a defensive re-check here means this function's own
/// correctness doesn't depend on every future caller reusing that filter.
pub fn resolve_selection<'a>(
    rows: &[RenderRow],
    files: &'a [DiffFile],
    selected: &[usize],
) -> Vec<SelectedLine<'a>> {
    selected
        .iter()
        .filter_map(|&flat_idx| {
            let Some(RenderRow::Line {
                file_idx,
                hunk_idx,
                row_idx,
            }) = rows.get(flat_idx).copied()
            else {
                return None;
            };
            let file = files.get(file_idx)?;
            let row = file.hunks.get(hunk_idx)?.rows.get(row_idx)?;
            Some(SelectedLine {
                path: file.display_path(),
                kind: row.kind,
                old_line: row.old_line,
                new_line: row.new_line,
                text: row.text.as_str(),
            })
        })
        .collect()
}

/// The plain-text payload [`format_diff_selection`] produces, plus the
/// counts `ui::mod::handle_action`'s success status line reports — kept as
/// fields here rather than recomputed at the call site, since
/// `format_diff_selection` consumes its input and the call site has
/// nothing left to recount from by the time it needs them.
#[derive(Debug, PartialEq, Eq)]
pub struct FormattedSelection {
    pub text: String,
    pub file_count: usize,
    pub line_count: usize,
    pub byte_count: usize,
}

/// Why [`format_diff_selection`] refused to produce a payload. Each variant
/// maps to one specific status-bar sentence in `ui::mod::handle_action`,
/// never a generic "yank failed".
#[derive(Debug, PartialEq, Eq)]
pub enum YankError {
    /// The selection was non-empty (or `App::toggle_visual` would never
    /// have started it), but every row inside the interval turned out to
    /// be structural — a header or a fold, never a `RenderRow::Line`.
    Empty,
    /// The formatted payload exceeds [`OSC52_MAX_BYTES`] before base64.
    /// Counted pre-encoding, same reasoning as [`write_osc52`]'s own bound:
    /// base64 inflates by roughly a third, so bounding the pre-encoded text
    /// is both the more honest number to show a reviewer and cheaper to
    /// compute than decoding back out of the encoded form.
    TooLarge { byte_count: usize },
}

/// The literal column header repeated at the top of every file group (issue
/// #17 req 4). Not derived from anything — the columns it names are fixed
/// by the format itself; a configurable output template is explicitly out
/// of scope (see the issue's "Out of scope" list).
const GROUP_HEADER: &str = "old:new | line";

/// The bare diff marker for one selected line. Deliberately its own
/// minimal mapping rather than reusing `diff_view`'s marker match, which
/// bundles the marker together with its on-screen `Color` — dead weight a
/// plain-text clipboard payload has no use for.
fn marker_char(kind: DiffLineKind) -> char {
    match kind {
        DiffLineKind::Add => '+',
        DiffLineKind::Del => '-',
        DiffLineKind::Context => ' ',
    }
}

/// Formats a resolved visual selection into the paste-ready plain-text
/// payload the issue's format specifies, or explains why it can't. Pure —
/// no I/O, no `App` — so this feature's formatting behavior is covered by
/// the unit tests below rather than needing a PTY.
///
/// Consumes `lines` rather than borrowing it: the only production caller
/// (`ui::mod::handle_action`) builds it from `app.rows`/`app.files` via
/// [`resolve_selection`], and letting this function own the `Vec` (and the
/// `&str`s it borrows from those two fields) ends that borrow the moment
/// this returns — before the caller needs `&mut app` again for
/// `App::cancel_visual` on success.
///
/// A new group starts whenever the path changes from the previous line,
/// repeating an earlier file's path/header block if the selection later
/// re-enters it rather than merging the two groups (req 4's "repeat a path
/// header ... rather than reordering"). Today's `flatten` walks files in a
/// fixed file → hunk → line order, so within one ascending selection an
/// already-visited file can never recur — this path is unreachable from
/// the main diff and is exercised below only via a hand-built fixture,
/// kept anyway because it's what the issue's contract actually specifies
/// and the cost of getting it right is one `!=` comparison.
///
/// One blank line separates two groups — never before the first or after
/// the last — a deliberate readability choice for a multi-file paste
/// target, not implied by anything else in the format. `file_count` counts
/// distinct paths, not header groups, so a (currently unreachable)
/// re-entrant selection still reports the true number of files touched.
///
/// The [`OSC52_MAX_BYTES`] bound is checked on the assembled plain text,
/// before base64 — matching [`write_osc52`]'s own pre-encoding bound.
pub fn format_diff_selection(
    lines: Vec<SelectedLine<'_>>,
) -> Result<FormattedSelection, YankError> {
    if lines.is_empty() {
        return Err(YankError::Empty);
    }
    let mut text = String::new();
    let mut last_path: Option<&str> = None;
    let mut seen_paths: HashSet<&str> = HashSet::new();
    for line in &lines {
        seen_paths.insert(line.path);
        if last_path != Some(line.path) {
            if last_path.is_some() {
                text.push('\n'); // blank line between groups, never before the first
            }
            text.push_str(line.path);
            text.push('\n');
            text.push_str(GROUP_HEADER);
            text.push('\n');
            last_path = Some(line.path);
        }
        let old = line
            .old_line
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".to_owned());
        let new = line
            .new_line
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".to_owned());
        text.push_str(&format!(
            "{old}:{new} | {}{}",
            marker_char(line.kind),
            line.text
        ));
        text.push('\n');
    }
    // Every row above ends with '\n'; drop the last one so the payload has
    // no trailing newline, matching the inspector's own `join("\n")`
    // convention for its copy payload.
    text.pop();

    let byte_count = text.len();
    if byte_count > OSC52_MAX_BYTES {
        return Err(YankError::TooLarge { byte_count });
    }
    Ok(FormattedSelection {
        text,
        file_count: seen_paths.len(),
        line_count: lines.len(),
        byte_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffHunk, DiffRow, flatten};

    fn diff_row(kind: DiffLineKind, text: &str, old: Option<u32>, new: Option<u32>) -> DiffRow {
        DiffRow {
            kind,
            text: text.to_owned(),
            old_line: old,
            new_line: new,
        }
    }

    fn file(rows: Vec<DiffRow>) -> DiffFile {
        DiffFile {
            hunks: vec![DiffHunk {
                rows,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn base64_encoding_is_terminal_safe() {
        assert_eq!(base64_encode(b"Katamari"), "S2F0YW1hcmk=");
        assert_eq!(base64_encode(b"\x00\xff"), "AP8=");
    }

    #[test]
    fn write_osc52_rejects_a_payload_over_the_bound() {
        let too_large = "x".repeat(OSC52_MAX_BYTES + 1);
        assert_eq!(
            write_osc52(&too_large).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn format_diff_selection_matches_the_issues_documented_example() {
        let lines = vec![
            SelectedLine {
                path: "src/lib.rs",
                kind: DiffLineKind::Context,
                old_line: Some(41),
                new_line: Some(42),
                text: "unchanged context",
            },
            SelectedLine {
                path: "src/lib.rs",
                kind: DiffLineKind::Add,
                old_line: None,
                new_line: Some(43),
                text: "added line",
            },
            SelectedLine {
                path: "src/lib.rs",
                kind: DiffLineKind::Del,
                old_line: Some(42),
                new_line: None,
                text: "deleted line",
            },
        ];
        let formatted = format_diff_selection(lines).expect("non-empty selection formats");
        assert_eq!(
            formatted.text,
            "src/lib.rs\n\
             old:new | line\n\
             41:42 |  unchanged context\n\
             -:43 | +added line\n\
             42:- | -deleted line"
        );
        assert_eq!(formatted.file_count, 1);
        assert_eq!(formatted.line_count, 3);
        assert_eq!(formatted.byte_count, formatted.text.len());
    }

    #[test]
    fn resolve_selection_uses_the_new_path_for_a_rename() {
        let files = vec![DiffFile {
            old_path: Some("old_name.rs".to_owned()),
            new_path: Some("new_name.rs".to_owned()),
            is_renamed: true,
            ..file(vec![diff_row(
                DiffLineKind::Context,
                "line",
                Some(1),
                Some(1),
            )])
        }];
        let rows = flatten(&files);
        let line_idx = rows
            .iter()
            .position(|row| matches!(row, RenderRow::Line { .. }))
            .unwrap();
        let resolved = resolve_selection(&rows, &files, &[line_idx]);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].path, "new_name.rs");
    }

    #[test]
    fn resolve_selection_uses_the_old_path_for_a_deletion() {
        let files = vec![DiffFile {
            old_path: Some("gone.rs".to_owned()),
            new_path: None,
            is_deleted: true,
            ..file(vec![diff_row(DiffLineKind::Del, "bye", Some(1), None)])
        }];
        let rows = flatten(&files);
        let line_idx = rows
            .iter()
            .position(|row| matches!(row, RenderRow::Line { .. }))
            .unwrap();
        let resolved = resolve_selection(&rows, &files, &[line_idx]);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].path, "gone.rs");
    }

    #[test]
    fn resolve_selection_skips_structural_rows_and_stale_indices() {
        let files = vec![DiffFile {
            new_path: Some("a.rs".to_owned()),
            ..file(vec![diff_row(
                DiffLineKind::Context,
                "one",
                Some(1),
                Some(1),
            )])
        }];
        let rows = flatten(&files);
        // Every non-`Line` index (FileHeader, HunkHeader, ...) plus one
        // wildly out-of-range index — none of these should ever appear in
        // the resolved output, and none should panic.
        let mut selected: Vec<usize> = (0..rows.len())
            .filter(|&idx| !matches!(rows[idx], RenderRow::Line { .. }))
            .collect();
        selected.push(rows.len() + 1000);
        let resolved = resolve_selection(&rows, &files, &selected);
        assert!(
            resolved.is_empty(),
            "no structural or stale index should resolve"
        );
    }

    #[test]
    fn multi_file_selection_preserves_screen_order_and_counts_each_file_once() {
        let files = vec![
            DiffFile {
                new_path: Some("a.rs".to_owned()),
                ..file(vec![diff_row(DiffLineKind::Add, "in a", None, Some(1))])
            },
            DiffFile {
                new_path: Some("b.rs".to_owned()),
                ..file(vec![diff_row(DiffLineKind::Add, "in b", None, Some(1))])
            },
        ];
        let rows = flatten(&files);
        let selected: Vec<usize> = (0..rows.len())
            .filter(|&idx| matches!(rows[idx], RenderRow::Line { .. }))
            .collect();
        let resolved = resolve_selection(&rows, &files, &selected);
        let formatted = format_diff_selection(resolved).unwrap();
        assert_eq!(formatted.file_count, 2);
        assert_eq!(formatted.line_count, 2);
        // "a.rs" must appear, in full, before "b.rs" — selection order, not
        // some other sort.
        let a_pos = formatted.text.find("a.rs").unwrap();
        let b_pos = formatted.text.find("b.rs").unwrap();
        assert!(a_pos < b_pos);
    }

    /// A selection that leaves a file and later re-enters it repeats the
    /// header rather than merging the two groups (req 4) — unreachable
    /// from `App::selected_rows()` today (see `format_diff_selection`'s
    /// docs on why), so this drives the pure function directly with a
    /// hand-built `Vec<SelectedLine>` instead of a flattened selection.
    #[test]
    fn a_selection_that_re_enters_a_file_repeats_its_header() {
        let lines = vec![
            SelectedLine {
                path: "a.rs",
                kind: DiffLineKind::Context,
                old_line: Some(1),
                new_line: Some(1),
                text: "first a",
            },
            SelectedLine {
                path: "b.rs",
                kind: DiffLineKind::Context,
                old_line: Some(1),
                new_line: Some(1),
                text: "only b",
            },
            SelectedLine {
                path: "a.rs",
                kind: DiffLineKind::Context,
                old_line: Some(2),
                new_line: Some(2),
                text: "second a",
            },
        ];
        let formatted = format_diff_selection(lines).unwrap();
        assert_eq!(
            formatted.text.matches("a.rs").count(),
            2,
            "the header repeats on re-entry rather than merging: {}",
            formatted.text
        );
        assert_eq!(
            formatted.file_count, 2,
            "file_count is distinct files, not header groups"
        );
        assert_eq!(formatted.line_count, 3);
    }

    #[test]
    fn empty_selection_reports_the_empty_error() {
        assert_eq!(format_diff_selection(Vec::new()), Err(YankError::Empty));
    }

    #[test]
    fn tabs_and_cjk_text_copy_byte_for_byte() {
        let lines = vec![SelectedLine {
            path: "notes.txt",
            kind: DiffLineKind::Context,
            old_line: Some(1),
            new_line: Some(1),
            text: "\tindented\tこんにちは世界",
        }];
        let formatted = format_diff_selection(lines).unwrap();
        assert!(
            formatted.text.contains("\tindented\tこんにちは世界"),
            "text must copy exactly, no trim/tab-expand: {}",
            formatted.text
        );
    }

    #[test]
    fn oversized_selection_reports_the_pre_base64_byte_count() {
        let long_text = "x".repeat(OSC52_MAX_BYTES + 10);
        let lines = vec![SelectedLine {
            path: "big.rs",
            kind: DiffLineKind::Add,
            old_line: None,
            new_line: Some(1),
            text: &long_text,
        }];
        match format_diff_selection(lines) {
            Err(YankError::TooLarge { byte_count }) => {
                assert!(byte_count > OSC52_MAX_BYTES);
                // Pre-base64: the reported count is the plain text's own
                // length, not anything base64 would have inflated it to.
                assert!(byte_count < OSC52_MAX_BYTES + 100);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }
}
