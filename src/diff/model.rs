//! Parses `git diff --no-color` unified-diff text into a structured tree, then
//! flattens that tree into a stable, indexable sequence of render rows.
//!
//! The two-stage split matters: parsing owns everything the diff format is
//! fussy about (headers, hunk ranges, rename/new/delete markers), while
//! flattening owns nothing but list order. Scrolling and navigation index
//! into the flat `Vec<RenderRow>` by position, so that vector's order is the
//! single source of truth for "what's currently on screen."

use std::borrow::Cow;

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
    /// Whether this hunk is its file's last and reaches the *new* side's
    /// true end of file — set by the parser only for the unambiguous case
    /// (a `\ No newline at end of file` marker following a row that exists
    /// on the new side; see [`parse_unified_diff`]'s handling of that
    /// marker), and by [`crate::ui::app::App`] during derivation once an
    /// expand has actually probed past this hunk and found nothing there
    /// (see `App`'s `trailing_probed_empty` tracking). An ordinary context
    /// diff gives no other way to tell "this hunk's neighborhood just
    /// wasn't near anything else that changed" apart from "the file
    /// genuinely ends here" — this field is what lets [`file_gaps`]
    /// suppress a trailing fold row once either signal has confirmed there
    /// is nothing left to hide.
    pub known_eof: bool,
}

/// A changed file's high-level status, for the sidebar's file-tree badge
/// (issue #15) — a small, closed classification rather than exposing
/// [`DiffFile`]'s three raw booleans directly, so a rendering site (or a
/// future consumer, e.g. a mouse tooltip) matches on one value instead of
/// re-deriving the same priority order [`DiffFile::status`] already
/// centralizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Renamed,
    Modified,
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

    /// This file's change kind, for [`Self::badge`] and anything else that
    /// needs the classification without the single-letter rendering
    /// attached. Priority `is_deleted` > `is_renamed` > `is_new` >
    /// `Modified` (the fallback for a plain content change, or for none of
    /// the three flags being set at all): `parse_unified_diff` only ever
    /// sets one of the three per real git diff, but every `DiffFile` this
    /// module's own tests construct builds one directly rather than through
    /// the parser, so a defensive order still matters — `is_deleted` wins
    /// over `is_renamed` because a reviewer needs to know the file is gone
    /// before they need to know what it used to be called, and git itself
    /// can't actually produce a diff with both flags set together to
    /// disagree with that ordering anyway.
    pub fn status(&self) -> FileStatus {
        if self.is_deleted {
            FileStatus::Deleted
        } else if self.is_renamed {
            FileStatus::Renamed
        } else if self.is_new {
            FileStatus::Added
        } else {
            FileStatus::Modified
        }
    }

    /// The single-letter status badge [`crate::ui::sidebar`] renders in the
    /// file tree's marker column, matching [`Self::status`]'s priority.
    pub fn badge(&self) -> char {
        match self.status() {
            FileStatus::Added => 'A',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Modified => 'M',
        }
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
                old_path: a,
                new_path: b,
                ..Default::default()
            });
            continue;
        }

        let Some(file) = current.as_mut() else {
            continue;
        };

        if let Some(rest) = line.strip_prefix("rename from ") {
            file.is_renamed = true;
            file.old_path = Some(decode_git_path(rest).into_owned());
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            file.is_renamed = true;
            file.new_path = Some(decode_git_path(rest).into_owned());
        } else if line.starts_with("new file mode") {
            file.is_new = true;
        } else if line.starts_with("deleted file mode") {
            file.is_deleted = true;
        } else if let Some(rest) = line.strip_prefix("--- ") {
            if rest == "/dev/null" {
                file.is_new = true;
                file.old_path = None;
            } else {
                file.old_path = Some(parse_file_header_path(rest));
            }
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            if rest == "/dev/null" {
                file.is_deleted = true;
                file.new_path = None;
            } else {
                file.new_path = Some(parse_file_header_path(rest));
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
                    known_eof: false,
                });
            }
        } else if line == "\\ No newline at end of file" {
            // Not a content row; the preceding row's text already lacks a
            // trailing newline because we split on `.lines()`. When that
            // row also carries a *new*-side line number (`Context`/`Add` —
            // a `Del`-only row only says the *old* file's last line lacked
            // one, which says nothing about where the new file ends),
            // that's the unambiguous signal this hunk reaches the new
            // file's true EOF, so `file_gaps` should never invent a
            // trailing fold row below it.
            if let Some(hunk) = current_hunk.as_mut()
                && matches!(hunk.rows.last(), Some(r) if r.new_line.is_some())
            {
                hunk.known_eof = true;
            }
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

/// Decode bytes before UTF-8: Git's octal escapes encode individual bytes,
/// not Unicode code points. Return the unused suffix to split diff --git
/// headers without mistaking spaces or escaped quotes inside a name.
fn decode_quoted_path(path: &str) -> Option<(String, &str)> {
    let bytes = path.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut decoded = Vec::new();
    let mut i = 1;
    while let Some(&byte) = bytes.get(i) {
        i += 1;
        match byte {
            b'"' => return Some((String::from_utf8(decoded).ok()?, &path[i..])),
            b'\\' => {
                let escape = *bytes.get(i)?;
                i += 1;
                decoded.push(match escape {
                    b'a' => 7,
                    b'b' => 8,
                    b'f' => 12,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'v' => 11,
                    b'\\' => b'\\',
                    b'"' => b'"',
                    b'0'..=b'3' => {
                        let second = *bytes.get(i)?;
                        let third = *bytes.get(i + 1)?;
                        if !(b'0'..=b'7').contains(&second) || !(b'0'..=b'7').contains(&third) {
                            return None;
                        }
                        i += 2;
                        (escape - b'0') * 64 + (second - b'0') * 8 + (third - b'0')
                    }
                    _ => return None,
                });
            }
            _ => decoded.push(byte),
        }
    }
    None
}

/// Unquoted paths are literal, including backslashes. Preserve malformed
/// quoting or non-UTF-8 names verbatim rather than inventing a different path.
fn decode_git_path(path: &str) -> Cow<'_, str> {
    match decode_quoted_path(path) {
        Some((decoded, "")) => Cow::Owned(decoded),
        _ => Cow::Borrowed(path),
    }
}

/// Git may append a tab delimiter after a ---/+++ pathname with spaces.
/// That delimiter is outside any quotes; rename metadata has no such suffix.
fn parse_file_header_path(path: &str) -> String {
    let path = decode_git_path(path.strip_suffix('\t').unwrap_or(path));
    strip_ab_prefix(&path).to_owned()
}

/// These paths are the only names available for binary/mode-only diffs.
/// Text and rename headers remain authoritative. Preserve the existing
/// unquoted-space handling while accepting either or both names quoted.
fn split_diff_git_paths(rest: &str) -> (Option<String>, Option<String>) {
    let (a, b) = if rest.starts_with('"') {
        let Some((a, tail)) = decode_quoted_path(rest) else {
            return (None, None);
        };
        let Some(b) = tail.strip_prefix(' ') else {
            return (None, None);
        };
        (Cow::Owned(a), decode_git_path(b))
    } else {
        let Some(separator) = rest.find(" \"b/").or_else(|| rest.find(" b/")) else {
            return (None, None);
        };
        (
            Cow::Borrowed(&rest[..separator]),
            decode_git_path(&rest[separator + 1..]),
        )
    };
    (
        Some(strip_ab_prefix(&a).to_owned()),
        Some(strip_ab_prefix(&b).to_owned()),
    )
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
/// `RenderRow` within the `Vec` returned by [`flatten`] is the position
/// [`crate::ui::app::App`]'s cursor/scroll state addresses — but only
/// stable *between* rederives, not for a parsed diff's whole lifetime: a
/// `z o`/`z c` fold toggle (or a watch refresh) calls `App::rederive`,
/// which re-flattens `files` from scratch and can shift every later row's
/// index or delete one outright — splicing a `Between`/`Trailing` gap
/// merges two hunks (see [`splice_gap`]), removing the second hunk's own
/// `HunkHeader` row from the very next `flatten()` call. Every rebuild
/// funnels through `App::rederive`, which re-establishes `cursor`/
/// `scroll_offset` deliberately rather than assuming an old index still
/// points at the same thing — anything that must survive a fold toggle (a
/// comment anchor, a diagnostic, a jump target) keys off content
/// (`display_path`, line number), never a raw `RenderRow` index held onto
/// across a rederive.
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
    /// A fold row standing in for a run of unchanged lines git omitted from
    /// the diff — `gap_idx` indexes [`file_gaps`]'s output for `file_idx`,
    /// recomputed fresh (not stored) every time a caller needs the gap's
    /// actual line range/offset, the same "structural, not cached"
    /// discipline the rest of this module already applies to
    /// hunk/line addressing. Cursor-addressable like any other row (`z o`
    /// expands the gap it stands on, `App::diff_file` highlights its
    /// file in the sidebar), but never a `Line` — hover/comments/
    /// diagnostics all key off `RenderRow::Line` alone, so a fold row is
    /// simply invisible to those systems until `z o` replaces it with real
    /// `Line` rows.
    Gap {
        file_idx: usize,
        gap_idx: usize,
    },
    /// Stands in for a whole hunk the reviewer has marked reviewed (`r`),
    /// the same way `Gap` stands in for a run of unchanged lines git
    /// omitted — see [`collapse_reviewed_hunks`]. Deliberately carries no
    /// line-count field of its own: the renderer reads
    /// `files[file_idx].hunks[hunk_idx].rows.len()` directly, one field
    /// access, matching this module's "structural, not cached" rule for
    /// [`Gap`] itself. Cursor-addressable like `Gap` (`z o` expands the
    /// hunk it stands on back to its ordinary `HunkHeader`+`Line` rows), but
    /// never a `Line` — hover/comments/diagnostics stay invisible to it
    /// until an expand replaces it with real rows again.
    ReviewedHunk {
        file_idx: usize,
        hunk_idx: usize,
    },
}

/// Flattens parsed files into the sequence the UI renders and scrolls
/// through, in file → hunk → line order, with a [`RenderRow::Gap`] spliced
/// in wherever [`file_gaps`] reports git omitted unchanged lines.
pub fn flatten(files: &[DiffFile]) -> Vec<RenderRow> {
    let mut rows = Vec::new();
    for (file_idx, file) in files.iter().enumerate() {
        rows.push(RenderRow::FileHeader { file_idx });
        if file.is_binary {
            rows.push(RenderRow::BinaryNotice { file_idx });
        }
        let gaps = file_gaps(file);
        let mut gap_idx = 0;
        if matches!(
            gaps.first(),
            Some(Gap {
                position: GapPosition::Leading,
                ..
            })
        ) {
            rows.push(RenderRow::Gap { file_idx, gap_idx });
            gap_idx += 1;
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
            let follows_this_hunk = matches!(
                gaps.get(gap_idx),
                Some(Gap {
                    position: GapPosition::Between(i) | GapPosition::Trailing(i),
                    ..
                }) if *i == hunk_idx
            );
            if follows_this_hunk {
                rows.push(RenderRow::Gap { file_idx, gap_idx });
                gap_idx += 1;
            }
        }
    }
    rows
}

/// Collapses every hunk whose `(file_idx, hunk_idx)` is in `reviewed` down
/// to one [`RenderRow::ReviewedHunk`] row, replacing the run of rows
/// [`flatten`] produced for it (its `HunkHeader` plus every `Line`
/// belonging to that hunk — always contiguous, since `flatten`'s own hunk
/// loop emits them that way). A pure `Vec<RenderRow>` transform: it needs no
/// `&[DiffFile]`, since every row already carries the indices a caller can
/// resolve content from later. [`RenderRow::Gap`] rows are untouched —
/// they sit *between* hunks positionally, never inside one, so collapsing a
/// neighboring hunk can never disturb one. An empty `reviewed` set is a
/// no-op that returns `rows` unchanged.
pub fn collapse_reviewed_hunks(
    rows: Vec<RenderRow>,
    reviewed: &std::collections::HashSet<(usize, usize)>,
) -> Vec<RenderRow> {
    if reviewed.is_empty() {
        return rows;
    }
    let mut out = Vec::with_capacity(rows.len());
    let mut rows = rows.into_iter().peekable();
    while let Some(row) = rows.next() {
        match row {
            RenderRow::HunkHeader { file_idx, hunk_idx }
                if reviewed.contains(&(file_idx, hunk_idx)) =>
            {
                while matches!(
                    rows.peek(),
                    Some(RenderRow::Line { file_idx: f, hunk_idx: h, .. })
                        if *f == file_idx && *h == hunk_idx
                ) {
                    rows.next();
                }
                out.push(RenderRow::ReviewedHunk { file_idx, hunk_idx });
            }
            other => out.push(other),
        }
    }
    out
}

/// Where one [`Gap`] sits relative to a file's hunks — which hunk(s), if
/// any, border it, and therefore where [`flatten`] inserts its
/// [`RenderRow::Gap`] and which hunk(s) [`splice_gap`] merges it into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapPosition {
    /// Before the first hunk (only possible when that hunk doesn't start at
    /// line 1 on either side).
    Leading,
    /// Between hunk `.0` and the hunk right after it.
    Between(usize),
    /// After hunk `.0`, the file's last, through EOF.
    Trailing(usize),
}

/// One run of unchanged lines git omitted from the diff — computed by pure
/// arithmetic over a file's hunk boundaries ([`file_gaps`]), never stored on
/// [`DiffFile`] itself, so it's always in lockstep with whatever hunk shape
/// the file currently has (post-splice included).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    pub position: GapPosition,
    /// 1-based new-side line where the hidden block starts.
    pub new_start: u32,
    /// 1-based new-side line where the hidden block ends, inclusive —
    /// `None` only for an unbounded trailing gap (nothing has probed the
    /// live file to find out how much further it actually goes; see
    /// `App::expand_gap`).
    pub new_end: Option<u32>,
    /// `old_line = new_line - offset` for every line in this gap: since
    /// nothing changes across a gap, the running difference between the two
    /// sides' line numbers stays exactly what it was the moment the
    /// preceding hunk (or, for the leading gap, the following one) ended —
    /// see this module's docs on why that's pinned to hunk-level fields
    /// rather than derived from any one row.
    pub offset: i64,
}

impl Gap {
    /// How many lines this gap hides, when known — `None` for an unbounded
    /// trailing gap, matching [`Self::new_end`].
    pub fn line_count(&self) -> Option<u32> {
        self.new_end.map(|end| end - self.new_start + 1)
    }
}

/// The last line number on one side (old or new — same formula for either,
/// so this takes a bare `(start, lines)` pair rather than a `DiffHunk`)
/// that a hunk's range actually reaches: ordinarily `start + lines - 1`,
/// the last of its `lines` covered lines. Per the unified-diff zero-count
/// convention, a hunk with `lines == 0` (a pure deletion with nothing
/// inserted on the new side, or a pure insertion with nothing removed on
/// the old side) has no covered line at all — `start` instead denotes the
/// anchor line the change is attached to, so it's already the answer with
/// no `- 1` to apply. [`side_after`] and [`file_gaps`]'s Leading/Between/
/// Trailing arithmetic are both built on top of this one formula rather
/// than each re-deriving the zero-count special case separately.
fn side_end(start: u32, lines: u32) -> u32 {
    if lines > 0 { start + lines - 1 } else { start }
}

/// One past [`side_end`] — the first line on this side that's untouched by
/// the hunk, immediately following whatever it covers (or, for a
/// zero-count side, following its anchor line). Where a Between/Trailing
/// gap's `new_start`/offset math starts counting from.
fn side_after(start: u32, lines: u32) -> u32 {
    side_end(start, lines) + 1
}

/// The line immediately before a hunk's range begins on this side — the
/// last untouched line preceding it, i.e. where a Leading gap ends.
/// Ordinarily `start - 1`. When `lines == 0`, [`side_end`]'s doc explains
/// why `start` itself is already the anchor line rather than one past it —
/// which means `start` is *also* already the last untouched line before
/// the hunk, not `start - 1` (the exact off-by-one that used to underflow
/// when `start == 0` and invert the range whenever `start == 1`).
fn side_before(start: u32, lines: u32) -> u32 {
    if lines > 0 { start - 1 } else { start }
}

/// The gaps a file's hunk boundaries imply, in file order (leading, then
/// each between-hunks gap, then trailing) — the per-file list
/// [`RenderRow::Gap::gap_idx`] indexes and [`flatten`] inserts rows from.
/// `&[]` for anything with no hunks to have a gap between (`is_binary`,
/// `is_new`, `is_deleted`, or a hunk-less rename) — none of those represent
/// "content git chose not to show," just content that never had any to
/// begin with.
///
/// Every boundary here goes through [`side_before`]/[`side_after`] rather
/// than the seemingly-simpler `start`/`start - 1`/`start + lines` directly,
/// so a hunk with a zero-count side (`diff.context=0`'s pure deletions and
/// insertions — see those functions' docs) computes the same correct
/// boundary an ordinary hunk does instead of an inverted or negative range.
/// A gap is only ever pushed when its range is non-empty (`new_before >=
/// new_after`-shaped checks below), which is also what keeps
/// [`Gap::line_count`] from ever underflowing on what this function hands
/// back.
pub fn file_gaps(file: &DiffFile) -> Vec<Gap> {
    if file.is_binary || file.is_new || file.is_deleted || file.hunks.is_empty() {
        return Vec::new();
    }
    let mut gaps = Vec::new();

    let first = &file.hunks[0];
    let new_before = side_before(first.new_start, first.new_lines);
    let old_before = side_before(first.old_start, first.old_lines);
    if new_before > 0 || old_before > 0 {
        gaps.push(Gap {
            position: GapPosition::Leading,
            new_start: 1,
            new_end: Some(new_before),
            offset: new_before as i64 - old_before as i64,
        });
    }

    for i in 0..file.hunks.len().saturating_sub(1) {
        let cur = &file.hunks[i];
        let next = &file.hunks[i + 1];
        let cur_new_after = side_after(cur.new_start, cur.new_lines);
        let next_new_before = side_before(next.new_start, next.new_lines);
        if next_new_before >= cur_new_after {
            let cur_old_after = side_after(cur.old_start, cur.old_lines);
            gaps.push(Gap {
                position: GapPosition::Between(i),
                new_start: cur_new_after,
                new_end: Some(next_new_before),
                offset: cur_new_after as i64 - cur_old_after as i64,
            });
        }
    }

    let last_idx = file.hunks.len() - 1;
    let last = &file.hunks[last_idx];
    if !last.known_eof {
        let new_after = side_after(last.new_start, last.new_lines);
        let old_after = side_after(last.old_start, last.old_lines);
        gaps.push(Gap {
            position: GapPosition::Trailing(last_idx),
            new_start: new_after,
            new_end: None,
            offset: new_after as i64 - old_after as i64,
        });
    }

    gaps
}

/// Which side of a [`Gap`] to look for a boundary row on — shared by the
/// disk-backed validation an interactive expand runs
/// ([`gap_boundary_matches`]) and the pristine-vs-pristine comparison a
/// watch refresh runs when deciding whether a previously expanded fold
/// still safely reapplies (`App`'s refresh-reapply pruning), so both walk a
/// gap's neighboring hunk exactly the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapSide {
    Above,
    Below,
}

/// The nearest row bordering `gap` on `side` that carries a *new*-side line
/// number (`Context` or `Add` — a pure-`Del` hunk edge, e.g. a
/// replaced-single-line file with zero context rows, has none), scanning
/// inward from that side's hunk boundary rather than just checking the
/// hunk's first/last row, since several `Del`-only rows can sit between the
/// true edge and the nearest row that actually has a new-side coordinate to
/// validate. `None` when that side has no bordering hunk at all (the
/// leading gap's `Above`, the trailing gap's `Below`) or the bordering hunk
/// has no new-side row anywhere in it.
pub fn gap_adjacent_line(
    file: &DiffFile,
    position: GapPosition,
    side: GapSide,
) -> Option<(u32, &str)> {
    let hunk_idx = match (position, side) {
        (GapPosition::Leading, GapSide::Above) => return None,
        (GapPosition::Trailing(_), GapSide::Below) => return None,
        (GapPosition::Leading, GapSide::Below) => 0,
        (GapPosition::Between(i) | GapPosition::Trailing(i), GapSide::Above) => i,
        (GapPosition::Between(i), GapSide::Below) => i + 1,
    };
    let hunk = file.hunks.get(hunk_idx)?;
    let row = match side {
        GapSide::Above => hunk.rows.iter().rev().find(|r| r.new_line.is_some()),
        GapSide::Below => hunk.rows.iter().find(|r| r.new_line.is_some()),
    }?;
    Some((
        row.new_line.expect("filtered by find above"),
        row.text.as_str(),
    ))
}

/// Whether `gap`'s boundary rows (whichever side(s) have one — see
/// [`gap_adjacent_line`]) still match `file_lines` (0-based, matching the
/// parser's own `.lines()` convention) at those rows' recorded new-side line
/// numbers. `true` when neither side has a boundary row to check (nothing
/// to contradict), which is the correct, permissive default rather than a
/// special case: a gap with no adjacent content row simply has nothing this
/// check can catch drift with. The one guard against expanding a gap whose
/// surrounding diff no longer matches what's actually on disk right now —
/// see `App::expand_gap`'s docs for why this, rather than re-verifying the
/// gap's entire hidden content, is the deliberate scope of this check.
pub fn gap_boundary_matches(file: &DiffFile, gap: &Gap, file_lines: &[&str]) -> bool {
    [GapSide::Above, GapSide::Below].into_iter().all(|side| {
        match gap_adjacent_line(file, gap.position, side) {
            Some((line, text)) => file_lines.get((line - 1) as usize) == Some(&text),
            None => true,
        }
    })
}

/// The ordinary `Context` rows a gap's hidden lines become once expanded,
/// numbered through [`Gap::offset`] — shared by a fresh interactive expand
/// (`texts` read live off disk) and a watch refresh reapplying a
/// previously expanded fold (`texts` read back from its cached lines), so
/// both number old/new lines through the exact same formula. `None` if
/// `offset` would ever put an old-side line below `1` — a malformed gap
/// (shouldn't happen for anything [`file_gaps`] itself produced) rather
/// than something worth panicking over.
pub fn context_rows_for_gap(gap: &Gap, texts: &[String]) -> Option<Vec<DiffRow>> {
    texts
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let new_line = gap.new_start + i as u32;
            let old_line = new_line as i64 - gap.offset;
            if old_line < 1 {
                return None;
            }
            Some(DiffRow {
                kind: DiffLineKind::Context,
                text: text.clone(),
                old_line: Some(old_line as u32),
                new_line: Some(new_line),
            })
        })
        .collect()
}

/// Splices `rows` (from [`context_rows_for_gap`]) into `file` at
/// `gap.position`, replacing the gap [`file_gaps`] reported there. Only the
/// four `u32` boundary fields change on whichever hunk(s) are touched —
/// never `header`, since the `@@ ... @@` text is formatted from those
/// fields at render time rather than stored (see `diff_view`'s/`main`'s
/// hunk-header rendering) — so there is no header string to rebuild, and a
/// `Between` merge keeps the earlier hunk's own header suffix rather than
/// the later hunk's.
pub fn splice_gap(file: &mut DiffFile, gap: &Gap, rows: Vec<DiffRow>) {
    let n = rows.len() as u32;
    match gap.position {
        GapPosition::Leading => {
            let hunk = &mut file.hunks[0];
            let mut merged = rows;
            merged.append(&mut hunk.rows);
            hunk.rows = merged;
            hunk.old_start = 1;
            hunk.new_start = 1;
            hunk.old_lines += n;
            hunk.new_lines += n;
        }
        GapPosition::Between(i) => {
            let next = file.hunks.remove(i + 1);
            let hunk = &mut file.hunks[i];
            hunk.rows.extend(rows);
            hunk.rows.extend(next.rows);
            hunk.old_lines = (next.old_start + next.old_lines) - hunk.old_start;
            hunk.new_lines = (next.new_start + next.new_lines) - hunk.new_start;
            hunk.known_eof = next.known_eof;
        }
        GapPosition::Trailing(i) => {
            let hunk = &mut file.hunks[i];
            hunk.rows.extend(rows);
            hunk.old_lines += n;
            hunk.new_lines += n;
            // A trailing splice always reads through the disk file's real
            // end — there is no other endpoint an unbounded gap could stop
            // at — so the hunk now provably reaches EOF. Without this,
            // `file_gaps` would emit a fresh trailing gap right below the
            // just-revealed lines, and expanding *that* dangling row would
            // take the empty-probe path and silently collapse the content
            // it appeared to extend.
            hunk.known_eof = true;
        }
    }
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
    use std::collections::HashSet;

    const MULTI_FILE_FIXTURE: &str = include_str!("fixtures/multi_file.diff");
    const JAPANESE_FIXTURE: &str = include_str!("fixtures/japanese.diff");
    const BINARY_FIXTURE: &str = include_str!("fixtures/binary.diff");

    /// GitHub uses Git's default quoting, unlike our local core.quotepath=false.
    /// Both spellings must describe exactly the same file and unchanged content.
    #[test]
    fn quoted_paths_match_unquoted_paths_for_text_and_binary_changes() {
        let quoted = r#""a/\346\227\245\346\234\254\350\252\236.rs" "b/\346\227\245\346\234\254\350\252\236.rs""#;
        let old = r#""a/\346\227\245\346\234\254\350\252\236.rs""#;
        let new = r#""b/\346\227\245\346\234\254\350\252\236.rs""#;
        for (metadata, old_header, new_header) in [
            ("", old, new),
            ("new file mode 100644\n", "/dev/null", new),
            ("deleted file mode 100644\n", old, "/dev/null"),
        ] {
            let escaped = format!(
                "diff --git {quoted}\n{metadata}--- {old_header}\n+++ {new_header}\n@@ -1 +1 @@\n-old\n+literal \\346\n"
            );
            let plain = escaped
                .replace(quoted, "a/日本語.rs b/日本語.rs")
                .replace(old, "a/日本語.rs")
                .replace(new, "b/日本語.rs");
            let files = parse_unified_diff(&escaped);
            assert_eq!(files, parse_unified_diff(&plain), "{escaped}");
            assert_eq!(files[0].display_path(), "日本語.rs");
            assert_eq!(files[0].hunks[0].rows[1].text, r"literal \346");
        }

        // Binary and mode-only diffs have no ---/+++ header to repair a
        // misparsed diff --git line, so that fallback must decode too.
        for metadata in [
            "Binary files old and new differ\n",
            "old mode 100644\nnew mode 100755\n",
        ] {
            let files = parse_unified_diff(&format!("diff --git {quoted}\n{metadata}"));
            assert_eq!(files[0].old_path.as_deref(), Some("日本語.rs"));
            assert_eq!(files[0].new_path.as_deref(), Some("日本語.rs"));
            assert!(files[0].hunks.is_empty());
        }
    }

    #[test]
    fn quoted_rename_metadata_preserves_real_a_and_b_directories() {
        let diff = r#"diff --git "a/a/\346\227\245\346\234\254\350\252\236.rs" "b/b/\346\227\245\346\234\254\350\252\236.rs"
similarity index 100%
rename from "a/\346\227\245\346\234\254\350\252\236.rs"
rename to "b/\346\227\245\346\234\254\350\252\236.rs"
"#;
        let files = parse_unified_diff(diff);
        assert_eq!(files[0].old_path.as_deref(), Some("a/日本語.rs"));
        assert_eq!(files[0].new_path.as_deref(), Some("b/日本語.rs"));
        assert!(files[0].is_renamed);
    }

    #[test]
    fn diff_header_handles_mixed_quoting_and_unquoted_spaces() {
        for header in [
            r#"a/old name.rs "b/new\tname.rs""#,
            r#""a/old\tname.rs" b/new name.rs"#,
            r#""a/old\"name.rs" "b/new\\name.rs""#,
        ] {
            let files = parse_unified_diff(&format!(
                "diff --git {header}\nold mode 100644\nnew mode 100755\n"
            ));
            let (old, new) = match header {
                h if h.starts_with("a/") => ("old name.rs", "new\tname.rs"),
                h if h.ends_with("b/new name.rs") => ("old\tname.rs", "new name.rs"),
                _ => ("old\"name.rs", "new\\name.rs"),
            };
            assert_eq!(files[0].old_path.as_deref(), Some(old));
            assert_eq!(files[0].new_path.as_deref(), Some(new));
        }
    }

    #[test]
    fn path_headers_decode_c_escapes_but_preserve_unquoted_text() {
        for (path, expected) in [
            (
                r#""a/bell\a\b\f\n\r\t\v\\\".txt""#,
                "bell\u{7}\u{8}\u{c}\n\r\t\u{b}\\\".txt",
            ),
            (
                r#""a/\346\227\245\346\234\254\350\252\236.txt""#,
                "日本語.txt",
            ),
            ("a/plain 日本語.txt\t", "plain 日本語.txt"),
            (r"a/literal\346.txt", r"literal\346.txt"),
            // Invalid quoting/UTF-8 must not panic or silently invent a path.
            (r#""a/\777.txt""#, r#""a/\777.txt""#),
            (r#""a/\377.txt""#, r#""a/\377.txt""#),
            (r#""a/\q.txt""#, r#""a/\q.txt""#),
            (r#""a/unterminated"#, r#""a/unterminated"#),
        ] {
            let files =
                parse_unified_diff(&format!("diff --git a/x b/x\n--- {path}\n+++ {path}\n"));
            assert_eq!(files[0].display_path(), expected, "{path}");
        }
    }

    #[test]
    fn parses_modified_file_hunk_with_line_numbers() {
        let files = parse_unified_diff(MULTI_FILE_FIXTURE);
        let modified = files
            .iter()
            .find(|f| f.display_path() == "src/lib.rs")
            .expect("modified file present");
        assert!(!modified.is_new && !modified.is_deleted && !modified.is_renamed);
        assert_eq!(modified.status(), FileStatus::Modified);
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
        assert_eq!(new_file.status(), FileStatus::Added);
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
        assert_eq!(deleted.status(), FileStatus::Deleted);
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
        assert_eq!(renamed.status(), FileStatus::Renamed);
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

    // -- Gap computation, splicing, and validation (fold rows) ------------

    fn hunk(old_start: u32, old_lines: u32, new_start: u32, new_lines: u32) -> DiffHunk {
        DiffHunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
            header: String::new(),
            known_eof: false,
            rows: (0..new_lines)
                .map(|i| DiffRow {
                    kind: DiffLineKind::Context,
                    text: format!("line {}", new_start + i),
                    old_line: Some(old_start + i),
                    new_line: Some(new_start + i),
                })
                .collect(),
        }
    }

    fn file_with_hunks(hunks: Vec<DiffHunk>) -> DiffFile {
        DiffFile {
            new_path: Some("f.txt".to_owned()),
            old_path: Some("f.txt".to_owned()),
            hunks,
            ..Default::default()
        }
    }

    #[test]
    fn adjacent_hunks_produce_no_between_gap() {
        // Hunk 0 covers new 1..=3, hunk 1 starts exactly at new 4 — nothing
        // hidden between them.
        let file = file_with_hunks(vec![hunk(1, 3, 1, 3), hunk(4, 3, 4, 3)]);
        let gaps = file_gaps(&file);
        assert!(
            gaps.iter()
                .all(|g| !matches!(g.position, GapPosition::Between(_))),
            "adjacent hunks must not produce a Between gap: {gaps:?}"
        );
    }

    #[test]
    fn leading_gap_appears_when_the_first_hunk_does_not_start_at_line_one() {
        let file = file_with_hunks(vec![hunk(10, 3, 10, 3)]);
        let gaps = file_gaps(&file);
        let leading = gaps
            .iter()
            .find(|g| g.position == GapPosition::Leading)
            .expect("leading gap present");
        assert_eq!(leading.new_start, 1);
        assert_eq!(leading.new_end, Some(9));
        assert_eq!(leading.offset, 0);
    }

    #[test]
    fn no_leading_gap_when_the_first_hunk_starts_at_line_one() {
        let file = file_with_hunks(vec![hunk(1, 3, 1, 3)]);
        let gaps = file_gaps(&file);
        assert!(!gaps.iter().any(|g| g.position == GapPosition::Leading));
    }

    #[test]
    fn trailing_gap_appears_for_an_ordinary_hunk_with_unknown_size() {
        let file = file_with_hunks(vec![hunk(1, 3, 1, 3)]);
        let gaps = file_gaps(&file);
        let trailing = gaps
            .iter()
            .find(|g| matches!(g.position, GapPosition::Trailing(0)))
            .expect("trailing gap present");
        assert_eq!(trailing.new_start, 4);
        assert_eq!(trailing.new_end, None);
        assert_eq!(trailing.line_count(), None);
    }

    #[test]
    fn known_eof_suppresses_the_trailing_gap() {
        let mut file = file_with_hunks(vec![hunk(1, 3, 1, 3)]);
        file.hunks[0].known_eof = true;
        let gaps = file_gaps(&file);
        assert!(
            !gaps
                .iter()
                .any(|g| matches!(g.position, GapPosition::Trailing(_))),
            "known_eof must suppress the trailing gap: {gaps:?}"
        );
    }

    #[test]
    fn the_no_newline_marker_after_a_new_side_row_sets_known_eof() {
        let diff = "diff --git a/f.txt b/f.txt\n\
index 1111111..2222222 100644\n\
--- a/f.txt\n\
+++ b/f.txt\n\
@@ -1,2 +1,2 @@\n\
 one\n\
-two\n\
+two changed\n\
\\ No newline at end of file\n";
        let files = parse_unified_diff(diff);
        assert!(
            files[0].hunks[0].known_eof,
            "an Add row's own EOF marker is unambiguous — new-file EOF"
        );
        assert!(
            file_gaps(&files[0])
                .iter()
                .all(|g| !matches!(g.position, GapPosition::Trailing(_)))
        );
    }

    #[test]
    fn the_no_newline_marker_after_a_del_only_row_does_not_set_known_eof() {
        // The marker follows the `-old` row, which has no new-side line at
        // all — it says the *old* file's last line lacked a newline, which
        // says nothing about where the *new* file ends.
        let diff = "diff --git a/f.txt b/f.txt\n\
index 1111111..2222222 100644\n\
--- a/f.txt\n\
+++ b/f.txt\n\
@@ -1,2 +1,1 @@\n\
 one\n\
-old last line\n\
\\ No newline at end of file\n";
        let files = parse_unified_diff(diff);
        assert!(!files[0].hunks[0].known_eof);
        assert!(
            file_gaps(&files[0])
                .iter()
                .any(|g| matches!(g.position, GapPosition::Trailing(_))),
            "without an unambiguous new-side EOF marker, the trailing gap must still render"
        );
    }

    #[test]
    fn new_is_deleted_is_binary_and_hunk_less_files_never_get_gaps() {
        let mut new_file = file_with_hunks(vec![hunk(10, 3, 10, 3)]);
        new_file.is_new = true;
        assert!(file_gaps(&new_file).is_empty());

        let mut deleted_file = file_with_hunks(vec![hunk(10, 3, 10, 3)]);
        deleted_file.is_deleted = true;
        assert!(file_gaps(&deleted_file).is_empty());

        let mut binary_file = file_with_hunks(vec![hunk(10, 3, 10, 3)]);
        binary_file.is_binary = true;
        assert!(file_gaps(&binary_file).is_empty());

        let hunk_less = file_with_hunks(vec![]);
        assert!(file_gaps(&hunk_less).is_empty());
    }

    #[test]
    fn splice_leading_prepends_to_the_first_hunk_and_resets_its_start() {
        let mut file = file_with_hunks(vec![hunk(10, 3, 10, 3)]);
        let gap = file_gaps(&file)
            .into_iter()
            .find(|g| g.position == GapPosition::Leading)
            .unwrap();
        let texts: Vec<String> = (1..=9).map(|n| format!("pre {n}")).collect();
        let rows = context_rows_for_gap(&gap, &texts).unwrap();
        assert_eq!(rows.len(), 9);
        splice_gap(&mut file, &gap, rows);

        assert_eq!(file.hunks.len(), 1);
        let h = &file.hunks[0];
        assert_eq!((h.old_start, h.new_start), (1, 1));
        assert_eq!((h.old_lines, h.new_lines), (12, 12));
        assert_eq!(h.rows[0].text, "pre 1");
        assert_eq!(h.rows[0].old_line, Some(1));
        assert_eq!(h.rows[0].new_line, Some(1));
        assert_eq!(h.rows[8].text, "pre 9");
        // The original hunk's own first row follows the spliced-in ones.
        assert_eq!(h.rows[9].text, "line 10");
    }

    #[test]
    fn splice_between_merges_two_hunks_and_keeps_the_earlier_headers_suffix() {
        let mut file = file_with_hunks(vec![hunk(1, 3, 1, 3), hunk(10, 3, 10, 3)]);
        file.hunks[0].header = "fn earlier()".to_owned();
        file.hunks[1].header = "fn later()".to_owned();
        let gap = file_gaps(&file)
            .into_iter()
            .find(|g| matches!(g.position, GapPosition::Between(0)))
            .unwrap();
        assert_eq!((gap.new_start, gap.new_end), (4, Some(9)));
        let texts: Vec<String> = (4..=9).map(|n| format!("mid {n}")).collect();
        let rows = context_rows_for_gap(&gap, &texts).unwrap();
        splice_gap(&mut file, &gap, rows);

        assert_eq!(file.hunks.len(), 1, "the two hunks merge into one");
        let h = &file.hunks[0];
        assert_eq!((h.old_start, h.old_lines), (1, 12));
        assert_eq!((h.new_start, h.new_lines), (1, 12));
        assert_eq!(
            h.header, "fn earlier()",
            "keeps the earlier hunk's own header suffix"
        );
        assert_eq!(h.rows.len(), 12);
        assert_eq!(h.rows[3].text, "mid 4");
        assert_eq!(h.rows[8].text, "mid 9");
        assert_eq!(
            h.rows[9].text, "line 10",
            "the later hunk's own rows follow"
        );
    }

    #[test]
    fn splice_trailing_appends_through_the_probed_end_of_file() {
        let mut file = file_with_hunks(vec![hunk(1, 3, 1, 3)]);
        let gap = file_gaps(&file)
            .into_iter()
            .find(|g| matches!(g.position, GapPosition::Trailing(0)))
            .unwrap();
        assert_eq!(gap.new_start, 4);
        let texts: Vec<String> = vec!["tail 4".to_owned(), "tail 5".to_owned()];
        let rows = context_rows_for_gap(&gap, &texts).unwrap();
        splice_gap(&mut file, &gap, rows);

        let h = &file.hunks[0];
        assert_eq!((h.old_lines, h.new_lines), (5, 5));
        assert_eq!(h.rows.len(), 5);
        assert_eq!(h.rows[3].text, "tail 4");
        assert_eq!(h.rows[4].text, "tail 5");
        assert_eq!(h.rows[4].new_line, Some(5));
    }

    #[test]
    fn splice_trailing_marks_eof_so_no_dangling_trailing_gap_reappears() {
        // Regression: without `known_eof = true` here, `file_gaps` emitted
        // a fresh unbounded trailing gap right below the just-spliced
        // lines, and expanding that dangling row took the empty-probe path
        // — silently collapsing the content the first press revealed.
        let mut file = file_with_hunks(vec![hunk(1, 3, 1, 3)]);
        let gap = file_gaps(&file)
            .into_iter()
            .find(|g| matches!(g.position, GapPosition::Trailing(0)))
            .unwrap();
        let texts: Vec<String> = vec!["tail 4".to_owned(), "tail 5".to_owned()];
        let rows = context_rows_for_gap(&gap, &texts).unwrap();
        splice_gap(&mut file, &gap, rows);

        assert!(file.hunks[0].known_eof, "a trailing splice reaches EOF");
        assert!(
            !file_gaps(&file)
                .iter()
                .any(|g| matches!(g.position, GapPosition::Trailing(_))),
            "no trailing gap may survive a trailing splice"
        );
    }

    #[test]
    fn context_rows_for_gap_over_an_empty_probe_produces_an_empty_splice() {
        // The pure half of a "trailing gap probed and found genuinely
        // empty" expand — zero lines in, zero `DiffRow`s out, not a
        // rejection.
        let gap = Gap {
            position: GapPosition::Trailing(0),
            new_start: 4,
            new_end: None,
            offset: 0,
        };
        let rows = context_rows_for_gap(&gap, &[]).unwrap();
        assert!(rows.is_empty());
    }

    /// The between-hunk offset formula (`(new_start + new_lines) -
    /// (old_start + old_lines)` of the *preceding* hunk) and the leading
    /// offset formula (`new_start - old_start` of the *following* hunk)
    /// must agree with each other on a fixture where the two sides have
    /// actually diverged (more adds than dels before the gap, so the
    /// running old/new offset isn't the trivial zero every same-length
    /// fixture above uses) — pinning down that both formulas track the
    /// same running difference rather than happening to coincide only when
    /// nothing has shifted.
    #[test]
    fn offset_formulas_agree_on_an_add_del_imbalanced_fixture() {
        // Hunk 0: old 1..=3 (3 lines) become new 1..=5 (5 lines) — net +2.
        // Hunk 1 (after a gap): starts at old 20, new 22 — consistent with
        // the +2 running offset hunk 0 leaves behind.
        let file = file_with_hunks(vec![hunk(1, 3, 1, 5), hunk(20, 3, 22, 3)]);
        let gaps = file_gaps(&file);
        let between = gaps
            .iter()
            .find(|g| matches!(g.position, GapPosition::Between(0)))
            .expect("between gap present");
        assert_eq!(between.offset, 2);

        // A leading gap on a *different* file whose only hunk mirrors
        // hunk 1's start here must derive the identical offset from the
        // opposite formula.
        let leading_file = file_with_hunks(vec![hunk(20, 3, 22, 3)]);
        let leading_gaps = file_gaps(&leading_file);
        let leading = leading_gaps
            .iter()
            .find(|g| g.position == GapPosition::Leading)
            .expect("leading gap present");
        assert_eq!(
            leading.offset, between.offset,
            "both formulas agree on the same running offset"
        );
    }

    // -- Zero-context (`diff.context=0`) hunks ----------------------------
    //
    // Real `git -c diff.context=0` output for a pure deletion or pure
    // insertion carries a `,0` count on the side that has nothing shown —
    // per the unified-diff spec, that side's `start` then denotes the
    // anchor line the change is attached to rather than one past its first
    // covered line. Built from `parse_unified_diff` directly (not the
    // synthetic `hunk()` helper above) so these also lock in that the
    // parser itself reads the shorthand/zero-count header forms git
    // actually emits — see `parse_hunk_header`/`parse_range`.

    #[test]
    fn shorthand_hunk_header_with_implied_single_line_counts_parses_correctly() {
        // Changing exactly one line with no surrounding context: both
        // sides omit `,1` — `@@ -6 +5 @@`, not `@@ -6,1 +5,1 @@`.
        let diff = "diff --git a/f.txt b/f.txt\n\
index 1111111..2222222 100644\n\
--- a/f.txt\n\
+++ b/f.txt\n\
@@ -6 +5 @@ e\n\
-f\n\
+X\n";
        let files = parse_unified_diff(diff);
        let hunk = &files[0].hunks[0];
        assert_eq!((hunk.old_start, hunk.old_lines), (6, 1));
        assert_eq!((hunk.new_start, hunk.new_lines), (5, 1));
        assert_eq!(hunk.header, "e");
    }

    #[test]
    fn zero_context_pure_deletion_mid_file_produces_a_valid_leading_gap_and_does_not_panic() {
        // Deleting line 2 of a 7-line file under `diff.context=0`:
        // `@@ -2 +1,0 @@` — old_start=2/old_lines=1 (shorthand, implied
        // count 1), new_start=1/new_lines=0 (explicit zero count).
        // Regression for the underflow panic this exact hunk shape used to
        // trigger in `Gap::line_count` whenever `new_start == 1` (see this
        // module's `side_before` docs).
        let diff = "diff --git a/f.txt b/f.txt\n\
index 1111111..2222222 100644\n\
--- a/f.txt\n\
+++ b/f.txt\n\
@@ -2 +1,0 @@\n\
-b\n";
        let files = parse_unified_diff(diff);
        let hunk = &files[0].hunks[0];
        assert_eq!((hunk.old_start, hunk.old_lines), (2, 1));
        assert_eq!((hunk.new_start, hunk.new_lines), (1, 0));

        let gaps = file_gaps(&files[0]);
        let leading = gaps
            .iter()
            .find(|g| g.position == GapPosition::Leading)
            .expect("one untouched line (new line 1) precedes this deletion");
        assert_eq!((leading.new_start, leading.new_end), (1, Some(1)));
        assert_eq!(leading.offset, 0);
        assert_eq!(
            leading.line_count(),
            Some(1),
            "must not underflow-panic on a zero-count new side"
        );
    }

    #[test]
    fn zero_context_deletion_at_the_very_top_of_file_emits_no_leading_gap() {
        // `new_start == 0` is git's zero-count convention for "the anchor
        // sits before line 1 entirely" — deleting the file's own first
        // line with no context. Must not read as "0 untouched lines
        // precede this, so new_end = -1" the way the pre-fix
        // `first.new_start - 1` formula would have.
        let diff = "diff --git a/f.txt b/f.txt\n\
index 1111111..2222222 100644\n\
--- a/f.txt\n\
+++ b/f.txt\n\
@@ -1 +0,0 @@\n\
-a\n";
        let files = parse_unified_diff(diff);
        let hunk = &files[0].hunks[0];
        assert_eq!((hunk.old_start, hunk.old_lines), (1, 1));
        assert_eq!((hunk.new_start, hunk.new_lines), (0, 0));

        let gaps = file_gaps(&files[0]);
        assert!(
            !gaps.iter().any(|g| g.position == GapPosition::Leading),
            "no leading gap: nothing precedes a deletion at the very top"
        );
    }

    #[test]
    fn zero_context_addition_only_hunk_produces_a_valid_leading_gap() {
        // `@@ -5,0 +6,3 @@`-shaped: inserting 3 new lines after old line
        // 5, nothing deleted — old_lines == 0 on this hunk's own side, the
        // mirror image of the pure-deletion case above.
        let diff = "diff --git a/f.txt b/f.txt\n\
index 1111111..2222222 100644\n\
--- a/f.txt\n\
+++ b/f.txt\n\
@@ -5,0 +6,3 @@\n\
+x\n\
+y\n\
+z\n";
        let files = parse_unified_diff(diff);
        let hunk = &files[0].hunks[0];
        assert_eq!((hunk.old_start, hunk.old_lines), (5, 0));
        assert_eq!((hunk.new_start, hunk.new_lines), (6, 3));

        let gaps = file_gaps(&files[0]);
        let leading = gaps
            .iter()
            .find(|g| g.position == GapPosition::Leading)
            .expect("5 untouched lines precede the insertion point");
        assert_eq!((leading.new_start, leading.new_end), (1, Some(5)));
        assert_eq!(leading.offset, 0, "nothing has shifted yet at this point");
    }

    #[test]
    fn gap_boundary_matches_accepts_disk_content_that_matches_the_hunk_edges() {
        let file = file_with_hunks(vec![hunk(1, 3, 1, 3), hunk(10, 3, 10, 3)]);
        let gap = file_gaps(&file)
            .into_iter()
            .find(|g| matches!(g.position, GapPosition::Between(0)))
            .unwrap();
        // The disk content agrees with both hunks' own edge rows ("line 3"
        // above the gap, "line 10" below it) — irrespective of what's in
        // between, which this check never inspects.
        let mut lines = vec![String::new(); 12];
        lines[0] = "line 1".to_owned();
        lines[1] = "line 2".to_owned();
        lines[2] = "line 3".to_owned();
        lines[9] = "line 10".to_owned();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        assert!(gap_boundary_matches(&file, &gap, &refs));
    }

    #[test]
    fn gap_boundary_matches_rejects_disk_content_that_drifted_from_the_hunk_edge() {
        let file = file_with_hunks(vec![hunk(1, 3, 1, 3), hunk(10, 3, 10, 3)]);
        let gap = file_gaps(&file)
            .into_iter()
            .find(|g| matches!(g.position, GapPosition::Between(0)))
            .unwrap();
        let mut lines = vec![String::new(); 12];
        lines[2] = "line 3 EDITED ON DISK".to_owned(); // drifted from the diff's own "line 3"
        lines[9] = "line 10".to_owned();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        assert!(!gap_boundary_matches(&file, &gap, &refs));
    }

    #[test]
    fn flatten_inserts_a_gap_row_between_two_non_adjacent_hunks() {
        let file = file_with_hunks(vec![hunk(1, 3, 1, 3), hunk(10, 3, 10, 3)]);
        let rows = flatten(std::slice::from_ref(&file));
        let gap_positions: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, RenderRow::Gap { .. }))
            .map(|(i, _)| i)
            .collect();
        // FileHeader, HunkHeader, 3 rows, Gap, HunkHeader, 3 rows, Gap(trailing)
        assert_eq!(gap_positions.len(), 2, "one Between gap, one Trailing gap");
        assert!(matches!(
            rows[gap_positions[0]],
            RenderRow::Gap { gap_idx: 0, .. }
        ));
        assert!(matches!(
            rows[gap_positions[1]],
            RenderRow::Gap { gap_idx: 1, .. }
        ));
    }

    // -- Reviewed-hunk collapsing --------------------------------------

    #[test]
    fn collapse_reviewed_hunks_replaces_one_hunks_rows_with_one_marker_row() {
        let file = file_with_hunks(vec![hunk(1, 3, 1, 3), hunk(10, 3, 10, 3)]);
        let rows = flatten(std::slice::from_ref(&file));
        let reviewed: HashSet<(usize, usize)> = [(0, 0)].into_iter().collect();
        let collapsed = collapse_reviewed_hunks(rows, &reviewed);

        // FileHeader, ReviewedHunk(0,0), Gap, HunkHeader(0,1), 3 lines,
        // Gap(trailing) — hunk 0's HunkHeader + 3 Lines became one row.
        assert!(matches!(
            collapsed[1],
            RenderRow::ReviewedHunk {
                file_idx: 0,
                hunk_idx: 0
            }
        ));
        assert!(
            !collapsed
                .iter()
                .any(|r| matches!(r, RenderRow::HunkHeader { hunk_idx: 0, .. })),
            "the reviewed hunk's own header must be gone"
        );
        assert!(
            collapsed
                .iter()
                .any(|r| matches!(r, RenderRow::HunkHeader { hunk_idx: 1, .. })),
            "the other, unreviewed hunk's header must survive"
        );
    }

    #[test]
    fn collapse_reviewed_hunks_leaves_a_neighboring_gap_row_untouched() {
        let file = file_with_hunks(vec![hunk(1, 3, 1, 3), hunk(10, 3, 10, 3)]);
        let rows = flatten(std::slice::from_ref(&file));
        let gaps_before = rows
            .iter()
            .filter(|r| matches!(r, RenderRow::Gap { .. }))
            .count();
        let reviewed: HashSet<(usize, usize)> = [(0, 0)].into_iter().collect();
        let collapsed = collapse_reviewed_hunks(rows, &reviewed);
        let gaps_after = collapsed
            .iter()
            .filter(|r| matches!(r, RenderRow::Gap { .. }))
            .count();
        assert_eq!(gaps_before, gaps_after, "gap rows must survive untouched");
    }

    #[test]
    fn collapse_reviewed_hunks_collapses_every_hunk_in_a_file_leaving_the_header() {
        let file = file_with_hunks(vec![hunk(1, 3, 1, 3), hunk(10, 3, 10, 3)]);
        let rows = flatten(std::slice::from_ref(&file));
        let reviewed: HashSet<(usize, usize)> = [(0, 0), (0, 1)].into_iter().collect();
        let collapsed = collapse_reviewed_hunks(rows, &reviewed);

        assert!(matches!(
            collapsed[0],
            RenderRow::FileHeader { file_idx: 0 }
        ));
        assert!(
            !collapsed
                .iter()
                .any(|r| matches!(r, RenderRow::HunkHeader { .. })
                    || matches!(r, RenderRow::Line { .. })),
            "no hunk header or line row should remain"
        );
        let marker_count = collapsed
            .iter()
            .filter(|r| matches!(r, RenderRow::ReviewedHunk { .. }))
            .count();
        assert_eq!(marker_count, 2);
    }

    #[test]
    fn collapse_reviewed_hunks_with_an_empty_set_is_a_no_op() {
        let file = file_with_hunks(vec![hunk(1, 3, 1, 3), hunk(10, 3, 10, 3)]);
        let rows = flatten(std::slice::from_ref(&file));
        let before = rows.clone();
        let collapsed = collapse_reviewed_hunks(rows, &HashSet::new());
        assert_eq!(before, collapsed);
    }

    // -- Status badges (issue #15) -----------------------------------------

    #[test]
    fn status_defaults_to_modified_when_no_flag_is_set() {
        let file = DiffFile {
            new_path: Some("src/lib.rs".to_owned()),
            old_path: Some("src/lib.rs".to_owned()),
            ..Default::default()
        };
        assert_eq!(file.status(), FileStatus::Modified);
        assert_eq!(file.badge(), 'M');
    }

    #[test]
    fn status_reads_added_deleted_and_renamed_flags() {
        let added = DiffFile {
            is_new: true,
            ..Default::default()
        };
        assert_eq!(added.status(), FileStatus::Added);
        assert_eq!(added.badge(), 'A');

        let deleted = DiffFile {
            is_deleted: true,
            ..Default::default()
        };
        assert_eq!(deleted.status(), FileStatus::Deleted);
        assert_eq!(deleted.badge(), 'D');

        let renamed = DiffFile {
            is_renamed: true,
            ..Default::default()
        };
        assert_eq!(renamed.status(), FileStatus::Renamed);
        assert_eq!(renamed.badge(), 'R');
    }

    /// `parse_unified_diff` only ever sets one of `is_new`/`is_deleted`/
    /// `is_renamed` per real diff, but [`DiffFile::status`]'s priority order
    /// still needs pinning down for whatever combination a directly
    /// constructed `DiffFile` (every other fixture in this module included)
    /// might carry — see that method's own docs for why `is_deleted` wins.
    #[test]
    fn status_prioritizes_deleted_over_renamed_over_added_when_multiple_flags_are_set() {
        let deleted_and_renamed = DiffFile {
            is_deleted: true,
            is_renamed: true,
            ..Default::default()
        };
        assert_eq!(deleted_and_renamed.status(), FileStatus::Deleted);

        let renamed_and_new = DiffFile {
            is_renamed: true,
            is_new: true,
            ..Default::default()
        };
        assert_eq!(renamed_and_new.status(), FileStatus::Renamed);

        let deleted_and_new = DiffFile {
            is_deleted: true,
            is_new: true,
            ..Default::default()
        };
        assert_eq!(deleted_and_new.status(), FileStatus::Deleted);
    }
}
