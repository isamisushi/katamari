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

#[cfg(test)]
mod tests {
    use super::*;

    const MULTI_FILE_FIXTURE: &str = include_str!("fixtures/multi_file.diff");
    const JAPANESE_FIXTURE: &str = include_str!("fixtures/japanese.diff");

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
}
