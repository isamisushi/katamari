//! Parses `git diff --no-color` unified-diff text into a structured tree, then
//! flattens that tree into a stable, indexable sequence of render rows.
//!
//! The two-stage split matters: parsing owns everything the diff format is
//! fussy about (headers, hunk ranges, rename/new/delete markers), while
//! flattening owns nothing but list order. Scrolling and navigation index
//! into the flat `Vec<RenderRow>` by position, so that vector's order is the
//! single source of truth for "what's currently on screen."

/// A single line within a hunk, classified by how it differs from HEAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Add,
    Del,
}

/// One line of a hunk body, with the `+`/`-`/` ` marker stripped from `text`
/// and both possible line numbers preserved (a pure add has no `old_line`, a
/// pure del has no `new_line`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    pub kind: DiffLineKind,
    pub text: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
}

/// A contiguous block of changed lines plus the surrounding context git
/// included, as delimited by an `@@ -old_start,old_lines +new_start,new_lines @@` header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// Full header text after the closing `@@`, e.g. a function signature
    /// git appends for context. Empty when there is none.
    pub header: String,
    pub rows: Vec<DiffRow>,
}

/// All hunks belonging to one file entry in the diff, plus the file-level
/// metadata git prints outside of any hunk (rename, new/deleted status).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffFile {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub hunks: Vec<DiffHunk>,
    pub is_new: bool,
    pub is_deleted: bool,
    pub is_renamed: bool,
    /// Set from git's `Binary files ... differ` notice. Binary files carry
    /// no hunks — `flatten` renders a single marker row for them instead of
    /// attempting to show content.
    pub is_binary: bool,
}

impl DiffFile {
    /// The path to show in the UI: the new path for anything still present,
    /// falling back to the old path for deletions.
    pub fn display_path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("<unknown>")
    }

    /// `(added, deleted)` line counts across all hunks, for the sidebar stat
    /// column.
    pub fn stat(&self) -> (u32, u32) {
        let mut added = 0;
        let mut deleted = 0;
        for hunk in &self.hunks {
            for row in &hunk.rows {
                match row.kind {
                    DiffLineKind::Add => added += 1,
                    DiffLineKind::Del => deleted += 1,
                    DiffLineKind::Context => {}
                }
            }
        }
        (added, deleted)
    }

    /// Whether this file's diff is expensive enough — by changed-line count,
    /// or by a lockfile-ish name regardless of size — that syntax
    /// highlighting (and, sharing the same call, LSP warm-up's `didOpen`)
    /// should be skipped in favor of plain styling. `max_changed_lines` is
    /// `config::highlight_max_lines()` at every real call site; threaded in
    /// explicitly here rather than read from `config` directly so this stays
    /// a pure, independently testable function of its own inputs, matching
    /// every other predicate in this module.
    pub fn skip_highlighting(&self, max_changed_lines: usize) -> bool {
        if is_lockfile_ish(self.display_path()) {
            return true;
        }
        let (added, deleted) = self.stat();
        (added + deleted) as usize > max_changed_lines
    }
}

/// Names that call for skipping syntax highlighting regardless of how small
/// the change is: lockfiles and minified bundles are enormous, mechanically
/// generated walls of text that a syntax tree never helps a reviewer read,
/// and are relatively expensive per byte for tree-sitter to tokenize for
/// zero benefit — a two-line change to a `package-lock.json` still means
/// tokenizing however many thousand bytes that one changed line happens to
/// be part of. `pub`, not just used by [`DiffFile::skip_highlighting`],
/// since [`crate::ui::file_view::FileView`] applies the identical name rule
/// to a whole opened file (which has no "changed lines" of its own to
/// threshold on) via [`crate::ui::file_view::FileView::with_hover_target`].
pub fn is_lockfile_ish(display_path: &str) -> bool {
    let name = display_path.rsplit('/').next().unwrap_or(display_path);
    name.ends_with(".lock") || name == "package-lock.json" || name.ends_with(".min.js")
}

/// Parses the output of `git diff --no-color --no-ext-diff` into structured
/// files. Unrecognized lines (blank separators, `index` lines, mode changes,
/// binary-file notices) are ignored rather than rejected, since M1 only
/// needs to render text diffs correctly, not validate git's output.
pub fn parse_unified_diff(text: &str) -> Vec<DiffFile> {
    let mut files = Vec::new();
    let mut current: Option<DiffFile> = None;
    let mut current_hunk: Option<DiffHunk> = None;
    let mut old_line_no: u32 = 0;
    let mut new_line_no: u32 = 0;

    macro_rules! flush_hunk {
        ($file:expr) => {
            if let Some(hunk) = current_hunk.take() {
                $file.hunks.push(hunk);
            }
        };
    }

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(mut file) = current.take() {
                flush_hunk!(file);
                files.push(file);
            }
            let (a, b) = split_diff_git_paths(rest);
            current = Some(DiffFile {
                old_path: a.map(str::to_owned),
                new_path: b.map(str::to_owned),
                ..Default::default()
            });
            continue;
        }

        let Some(file) = current.as_mut() else {
            continue;
        };

        if let Some(rest) = line.strip_prefix("rename from ") {
            file.is_renamed = true;
            file.old_path = Some(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            file.is_renamed = true;
            file.new_path = Some(rest.to_owned());
        } else if line.starts_with("new file mode") {
            file.is_new = true;
        } else if line.starts_with("deleted file mode") {
            file.is_deleted = true;
        } else if let Some(rest) = line.strip_prefix("--- ") {
            if rest == "/dev/null" {
                file.is_new = true;
                file.old_path = None;
            } else {
                file.old_path = Some(strip_ab_prefix(rest).to_owned());
            }
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            if rest == "/dev/null" {
                file.is_deleted = true;
                file.new_path = None;
            } else {
                file.new_path = Some(strip_ab_prefix(rest).to_owned());
            }
        } else if line.starts_with("Binary files ") && line.ends_with(" differ") {
            file.is_binary = true;
        } else if line.starts_with("@@ ") {
            flush_hunk!(file);
            if let Some((old_start, old_lines, new_start, new_lines, header)) =
                parse_hunk_header(line)
            {
                old_line_no = old_start;
                new_line_no = new_start;
                current_hunk = Some(DiffHunk {
                    old_start,
                    old_lines,
                    new_start,
                    new_lines,
                    header,
                    rows: Vec::new(),
                });
            }
        } else if line == "\\ No newline at end of file" {
            // Not a content row; the preceding row's text already lacks a
            // trailing newline because we split on `.lines()`.
        } else if let Some(hunk) = current_hunk.as_mut() {
            match line.as_bytes().first() {
                Some(b' ') => {
                    hunk.rows.push(DiffRow {
                        kind: DiffLineKind::Context,
                        text: line[1..].to_owned(),
                        old_line: Some(old_line_no),
                        new_line: Some(new_line_no),
                    });
                    old_line_no += 1;
                    new_line_no += 1;
                }
                Some(b'+') => {
                    hunk.rows.push(DiffRow {
                        kind: DiffLineKind::Add,
                        text: line[1..].to_owned(),
                        old_line: None,
                        new_line: Some(new_line_no),
                    });
                    new_line_no += 1;
                }
                Some(b'-') => {
                    hunk.rows.push(DiffRow {
                        kind: DiffLineKind::Del,
                        text: line[1..].to_owned(),
                        old_line: Some(old_line_no),
                        new_line: None,
                    });
                    old_line_no += 1;
                }
                _ => {
                    // Not a hunk body line (e.g. an empty line git printed
                    // between sections); ignore rather than misparse.
                }
            }
        }
    }

    if let Some(mut file) = current.take() {
        flush_hunk!(file);
        files.push(file);
    }

    files
}

/// Strips a diff's leading `a/` or `b/` path prefix, if present. Paths
/// outside a repo (rare, but possible with `--src-prefix`/`--dst-prefix`)
/// pass through unchanged.
fn strip_ab_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

/// Parses the `a/<path> b/<path>` tail of a `diff --git` line. This is only
/// a fallback: `---`/`+++`/`rename from`/`rename to` lines are authoritative
/// and overwrite whatever this finds. Falls back to `(None, None)` for
/// unusual quoting git applies to paths containing spaces or special
/// characters, since those cases are resolved by the authoritative lines
/// anyway.
fn split_diff_git_paths(rest: &str) -> (Option<&str>, Option<&str>) {
    match rest.split_once(" b/") {
        Some((a, b)) => (a.strip_prefix("a/").or(Some(a)), Some(b)),
        None => (None, None),
    }
}

/// Parses `@@ -old_start,old_lines +new_start,new_lines @@ trailing text`.
/// The `,lines` part is optional in git's output when the count is 1.
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32, String)> {
    let rest = line.strip_prefix("@@ ")?;
    let (ranges, trailing) = rest.split_once(" @@")?;
    let mut parts = ranges.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let (old_start, old_lines) = parse_range(old);
    let (new_start, new_lines) = parse_range(new);
    Some((
        old_start,
        old_lines,
        new_start,
        new_lines,
        trailing.trim_start().to_owned(),
    ))
}

/// Parses a hunk range component like `12,7` or `12` (count defaults to 1).
fn parse_range(s: &str) -> (u32, u32) {
    match s.split_once(',') {
        Some((start, count)) => (start.parse().unwrap_or(0), count.parse().unwrap_or(1)),
        None => (s.parse().unwrap_or(0), 1),
    }
}

/// One entry in the flattened, indexable render sequence. The index of a
/// `RenderRow` within the `Vec` returned by [`flatten`] is the stable
/// position used for cursor/scroll state — it never changes for the
/// lifetime of a parsed diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderRow {
    FileHeader {
        file_idx: usize,
    },
    /// A binary file's content marker — stands in for the hunks a binary
    /// file never has.
    BinaryNotice {
        file_idx: usize,
    },
    HunkHeader {
        file_idx: usize,
        hunk_idx: usize,
    },
    Line {
        file_idx: usize,
        hunk_idx: usize,
        row_idx: usize,
    },
}

/// Flattens parsed files into the sequence the UI renders and scrolls
/// through, in file → hunk → line order.
pub fn flatten(files: &[DiffFile]) -> Vec<RenderRow> {
    let mut rows = Vec::new();
    for (file_idx, file) in files.iter().enumerate() {
        rows.push(RenderRow::FileHeader { file_idx });
        if file.is_binary {
            rows.push(RenderRow::BinaryNotice { file_idx });
        }
        for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
            rows.push(RenderRow::HunkHeader { file_idx, hunk_idx });
            for row_idx in 0..hunk.rows.len() {
                rows.push(RenderRow::Line {
                    file_idx,
                    hunk_idx,
                    row_idx,
                });
            }
        }
    }
    rows
}

/// One cell of a [`SideBySideRow`]: either a line that exists on this side,
/// addressed by its position in [`flatten`]'s output (so cursor
/// highlighting and scrolling stay keyed to the same index space the
/// unified view uses), or [`SideCell::Empty`] when the other side has no
/// counterpart here — a pure add has nothing to show on the old side, and
/// vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideCell {
    Line { flat_idx: usize },
    Empty,
}

impl SideCell {
    fn flat_idx(self) -> Option<usize> {
        match self {
            SideCell::Line { flat_idx } => Some(flat_idx),
            SideCell::Empty => None,
        }
    }
}

/// One row of the side-by-side layout: a file/hunk/binary header spanning
/// both columns, or a pair of old/new cells within a hunk body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideBySideRow {
    Full { flat_idx: usize },
    Paired { old: SideCell, new: SideCell },
}

impl SideBySideRow {
    /// The largest flat-row index referenced by this row. Used by
    /// [`side_by_side_scroll_start`] to find where a unified-space scroll
    /// offset lands in side-by-side space.
    fn max_flat_idx(self) -> usize {
        match self {
            SideBySideRow::Full { flat_idx } => flat_idx,
            SideBySideRow::Paired { old, new } => old
                .flat_idx()
                .into_iter()
                .chain(new.flat_idx())
                .max()
                .unwrap_or(0),
        }
    }
}

/// Pairs up del/add runs within each hunk into row-aligned old/new columns,
/// for the side-by-side layout. Context lines appear identically on both
/// sides; a hunk's deleted lines and inserted lines are each a contiguous
/// run (unified diff format always lists a change block's deletions before
/// its insertions), so consecutive runs are zipped index-by-index with
/// [`SideCell::Empty`] filling out whichever run is shorter.
///
/// Every cell addresses back into [`flatten`]'s output by index rather than
/// duplicating line data, so a cursor position computed against the unified
/// row list highlights the correct cell here too.
pub fn flatten_side_by_side(files: &[DiffFile]) -> Vec<SideBySideRow> {
    let rows = flatten(files);
    let mut out = Vec::with_capacity(rows.len());
    let mut i = 0;
    while i < rows.len() {
        let Some(kind) = line_kind(files, &rows, i) else {
            out.push(SideBySideRow::Full { flat_idx: i });
            i += 1;
            continue;
        };

        if kind == DiffLineKind::Context {
            out.push(SideBySideRow::Paired {
                old: SideCell::Line { flat_idx: i },
                new: SideCell::Line { flat_idx: i },
            });
            i += 1;
            continue;
        }

        let del_start = i;
        let mut j = i;
        while line_kind(files, &rows, j) == Some(DiffLineKind::Del) {
            j += 1;
        }
        let add_start = j;
        while line_kind(files, &rows, j) == Some(DiffLineKind::Add) {
            j += 1;
        }
        let del_len = add_start - del_start;
        let add_len = j - add_start;
        for k in 0..del_len.max(add_len) {
            let old = if k < del_len {
                SideCell::Line {
                    flat_idx: del_start + k,
                }
            } else {
                SideCell::Empty
            };
            let new = if k < add_len {
                SideCell::Line {
                    flat_idx: add_start + k,
                }
            } else {
                SideCell::Empty
            };
            out.push(SideBySideRow::Paired { old, new });
        }
        i = j;
    }
    out
}

fn line_kind(files: &[DiffFile], rows: &[RenderRow], idx: usize) -> Option<DiffLineKind> {
    match rows.get(idx)? {
        RenderRow::Line {
            file_idx,
            hunk_idx,
            row_idx,
        } => Some(files[*file_idx].hunks[*hunk_idx].rows[*row_idx].kind),
        _ => None,
    }
}

/// Maps a unified-space scroll offset (an index into [`flatten`]'s output)
/// to the first [`SideBySideRow`] that reaches at least that far, so the
/// side-by-side view scrolls in lockstep with the unified one. Comparing
/// against each row's *maximum* flat index (rather than its minimum) is
/// what keeps the cursor's row on screen even when the cursor sits on the
/// later of a row's two cells (e.g. scrolled exactly to an add line whose
/// paired del line has a smaller index) — a minimum-based comparison would
/// skip past that row and scroll the cursor off the top.
pub fn side_by_side_scroll_start(rows: &[SideBySideRow], flat_scroll_offset: usize) -> usize {
    rows.iter()
        .position(|row| row.max_flat_idx() >= flat_scroll_offset)
        .unwrap_or_else(|| rows.len().saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTI_FILE_FIXTURE: &str = include_str!("fixtures/multi_file.diff");
    const JAPANESE_FIXTURE: &str = include_str!("fixtures/japanese.diff");
    const BINARY_FIXTURE: &str = include_str!("fixtures/binary.diff");

    #[test]
    fn parses_modified_file_hunk_with_line_numbers() {
        let files = parse_unified_diff(MULTI_FILE_FIXTURE);
        let modified = files
            .iter()
            .find(|f| f.display_path() == "src/lib.rs")
            .expect("modified file present");
        assert!(!modified.is_new && !modified.is_deleted && !modified.is_renamed);
        assert_eq!(modified.hunks.len(), 1);
        let hunk = &modified.hunks[0];
        assert_eq!((hunk.old_start, hunk.old_lines), (1, 4));
        assert_eq!((hunk.new_start, hunk.new_lines), (1, 5));

        let kinds: Vec<DiffLineKind> = hunk.rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiffLineKind::Context,
                DiffLineKind::Del,
                DiffLineKind::Add,
                DiffLineKind::Add,
                DiffLineKind::Context,
                DiffLineKind::Context,
            ]
        );

        let del = &hunk.rows[1];
        assert_eq!(del.old_line, Some(2));
        assert_eq!(del.new_line, None);
        assert_eq!(del.text, "fn old_name() {}");

        let add = &hunk.rows[2];
        assert_eq!(add.old_line, None);
        assert_eq!(add.new_line, Some(2));
        assert_eq!(add.text, "fn new_name() {}");

        let trailing_context = &hunk.rows[5];
        assert_eq!(trailing_context.old_line, Some(4));
        assert_eq!(trailing_context.new_line, Some(5));
    }

    #[test]
    fn parses_new_file() {
        let files = parse_unified_diff(MULTI_FILE_FIXTURE);
        let new_file = files
            .iter()
            .find(|f| f.display_path() == "src/new_module.rs")
            .expect("new file present");
        assert!(new_file.is_new);
        assert!(!new_file.is_deleted);
        assert_eq!(new_file.old_path, None);
        let (added, deleted) = new_file.stat();
        assert_eq!((added, deleted), (2, 0));
    }

    #[test]
    fn parses_deleted_file() {
        let files = parse_unified_diff(MULTI_FILE_FIXTURE);
        let deleted = files
            .iter()
            .find(|f| f.display_path() == "src/old_module.rs")
            .expect("deleted file present");
        assert!(deleted.is_deleted);
        assert!(!deleted.is_new);
        let (added, removed) = deleted.stat();
        assert_eq!((added, removed), (0, 2));
    }

    #[test]
    fn parses_renamed_file() {
        let files = parse_unified_diff(MULTI_FILE_FIXTURE);
        let renamed = files
            .iter()
            .find(|f| f.is_renamed)
            .expect("renamed file present");
        assert_eq!(renamed.old_path.as_deref(), Some("src/renamed_from.rs"));
        assert_eq!(renamed.new_path.as_deref(), Some("src/renamed_to.rs"));
    }

    #[test]
    fn handles_no_newline_at_eof_marker() {
        let files = parse_unified_diff(MULTI_FILE_FIXTURE);
        let file = files
            .iter()
            .find(|f| f.display_path() == "src/no_trailing_newline.rs")
            .expect("no-newline file present");
        let hunk = &file.hunks[0];
        let last = hunk.rows.last().expect("has rows");
        assert_eq!(last.text, "no trailing newline here");
        assert_eq!(last.kind, DiffLineKind::Add);
    }

    #[test]
    fn parses_japanese_content_without_corrupting_lines() {
        let files = parse_unified_diff(JAPANESE_FIXTURE);
        assert_eq!(files.len(), 1);
        let hunk = &files[0].hunks[0];
        let added = hunk
            .rows
            .iter()
            .find(|r| r.kind == DiffLineKind::Add)
            .expect("added row present");
        assert_eq!(added.text, "    // 日本語のコメントを追加する");
    }

    #[test]
    fn flatten_produces_stable_file_hunk_line_order() {
        let files = parse_unified_diff(MULTI_FILE_FIXTURE);
        let rows = flatten(&files);
        assert!(matches!(rows[0], RenderRow::FileHeader { file_idx: 0 }));
        let first_hunk_pos = rows
            .iter()
            .position(|r| matches!(r, RenderRow::HunkHeader { file_idx: 0, .. }))
            .expect("first file has a hunk header");
        assert!(matches!(
            rows[first_hunk_pos + 1],
            RenderRow::Line {
                file_idx: 0,
                hunk_idx: 0,
                row_idx: 0
            }
        ));
    }

    #[test]
    fn parses_binary_file_notice_without_hunks() {
        let files = parse_unified_diff(BINARY_FIXTURE);
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert!(file.is_binary);
        assert!(file.is_new);
        assert!(file.hunks.is_empty());
    }

    #[test]
    fn flatten_inserts_binary_notice_row_after_file_header() {
        let files = parse_unified_diff(BINARY_FIXTURE);
        let rows = flatten(&files);
        assert_eq!(
            rows,
            vec![
                RenderRow::FileHeader { file_idx: 0 },
                RenderRow::BinaryNotice { file_idx: 0 },
            ]
        );
    }

    /// The `src/lib.rs` hunk in [`MULTI_FILE_FIXTURE`] has kinds
    /// `[Context, Del, Add, Add, Context, Context]` — one deleted line
    /// followed by two added lines is exactly the "uneven run" case the
    /// side-by-side layout must fill with a blank cell rather than
    /// misaligning the columns.
    #[test]
    fn side_by_side_pairs_del_and_add_runs_with_filler_for_uneven_lengths() {
        let files = parse_unified_diff(MULTI_FILE_FIXTURE);
        let rows = flatten(&files);
        let hunk_header_idx = rows
            .iter()
            .position(|r| matches!(r, RenderRow::HunkHeader { file_idx: 0, .. }))
            .expect("lib.rs has a hunk");
        let ctx1 = hunk_header_idx + 1;
        let del = hunk_header_idx + 2;
        let add1 = hunk_header_idx + 3;
        let add2 = hunk_header_idx + 4;
        let ctx2 = hunk_header_idx + 5;

        let paired = flatten_side_by_side(&files);
        let start = paired
            .iter()
            .position(|r| {
                matches!(r, SideBySideRow::Paired { old: SideCell::Line { flat_idx }, .. } if *flat_idx == ctx1)
            })
            .expect("context row present in side-by-side output");

        assert_eq!(
            &paired[start..start + 4],
            &[
                SideBySideRow::Paired {
                    old: SideCell::Line { flat_idx: ctx1 },
                    new: SideCell::Line { flat_idx: ctx1 },
                },
                SideBySideRow::Paired {
                    old: SideCell::Line { flat_idx: del },
                    new: SideCell::Line { flat_idx: add1 },
                },
                SideBySideRow::Paired {
                    old: SideCell::Empty,
                    new: SideCell::Line { flat_idx: add2 },
                },
                SideBySideRow::Paired {
                    old: SideCell::Line { flat_idx: ctx2 },
                    new: SideCell::Line { flat_idx: ctx2 },
                },
            ]
        );
    }

    #[test]
    fn side_by_side_preserves_file_and_hunk_headers_as_full_width_rows() {
        let files = parse_unified_diff(MULTI_FILE_FIXTURE);
        let paired = flatten_side_by_side(&files);
        assert_eq!(paired[0], SideBySideRow::Full { flat_idx: 0 });
    }

    #[test]
    fn side_by_side_scroll_start_lands_on_the_row_whose_later_cell_covers_the_offset() {
        let files = parse_unified_diff(MULTI_FILE_FIXTURE);
        let rows = flatten(&files);
        let hunk_header_idx = rows
            .iter()
            .position(|r| matches!(r, RenderRow::HunkHeader { file_idx: 0, .. }))
            .expect("lib.rs has a hunk");
        let add2 = hunk_header_idx + 4;

        let paired = flatten_side_by_side(&files);
        // Scrolling to add2's flat index must not skip the paired row it
        // belongs to, even though that row's old-side cell (Empty) has no
        // flat index of its own and the row's del-side sibling is earlier.
        let start = side_by_side_scroll_start(&paired, add2);
        assert_eq!(
            paired[start],
            SideBySideRow::Paired {
                old: SideCell::Empty,
                new: SideCell::Line { flat_idx: add2 },
            }
        );
    }

    fn file_with_changed_lines(path: &str, added: u32, deleted: u32) -> DiffFile {
        let mut rows = Vec::new();
        for n in 0..added {
            rows.push(DiffRow {
                kind: DiffLineKind::Add,
                text: format!("added {n}"),
                old_line: None,
                new_line: Some(n),
            });
        }
        for n in 0..deleted {
            rows.push(DiffRow {
                kind: DiffLineKind::Del,
                text: format!("deleted {n}"),
                old_line: Some(n),
                new_line: None,
            });
        }
        DiffFile {
            new_path: Some(path.to_owned()),
            hunks: vec![DiffHunk {
                rows,
                ..DiffHunk::default()
            }],
            ..DiffFile::default()
        }
    }

    #[test]
    fn skip_highlighting_is_false_for_a_small_diff_on_an_ordinary_file() {
        let file = file_with_changed_lines("src/main.rs", 3, 2);
        assert!(!file.skip_highlighting(5000));
    }

    #[test]
    fn skip_highlighting_is_true_once_changed_lines_exceed_the_threshold() {
        let file = file_with_changed_lines("src/generated.rs", 4000, 4000);
        assert!(file.skip_highlighting(5000));
    }

    #[test]
    fn skip_highlighting_is_true_for_lockfile_ish_names_regardless_of_size() {
        for path in [
            "Cargo.lock",
            "package-lock.json",
            "vendor/bundle.min.js",
            "yarn.lock",
        ] {
            let file = file_with_changed_lines(path, 1, 0);
            assert!(
                file.skip_highlighting(5000),
                "{path} should skip highlighting regardless of its tiny diff"
            );
        }
    }
}
