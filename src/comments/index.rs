//! Turns a loaded comment list into something a diff render pass can look up
//! by `(file, line)` in O(1) — the piece that sits between
//! [`super::store::CommentStore::load`] (which knows nothing about a diff's
//! current line numbers) and `ui::diff_view` (which knows nothing about
//! comment relocation). Building the index is where [`super::relocate_range`]
//! actually runs, once per comment per reload rather than once per row per
//! frame.

use super::{Comment, Status, relocate_range};
use std::collections::HashMap;
use std::path::Path;

/// A corrupted or hand-edited `end_anchor` (e.g. an end line thousands of
/// lines past the start, surviving relocation because both endpoints happen
/// to still exist) must not make [`build_index`] materialize a marker entry
/// per line all the way out — that's an unbounded allocation driven by
/// untrusted file content. This caps how many `by_location` marker entries
/// one range ever produces; the range's *body* (in `starting_at`) is
/// unaffected; and rendering a range this long in the TUI's gutter/body
/// block is #19's problem, not this index's — a bound here just keeps
/// `build_index` itself cheap and finite.
const MAX_RANGE_MARKER_LINES: u32 = 500;

/// One comment's rendering-relevant state at whatever line(s) it currently
/// maps to — everything `ui::diff_view` needs to draw a gutter marker and an
/// inline body block without reaching back into the full [`Comment`] or
/// re-running [`relocate_range`] itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentAnnotation {
    pub id: String,
    pub status: Status,
    pub body: String,
    /// `true` when [`relocate_range`] couldn't place the comment on the
    /// current file — this annotation is shown at its *original* anchor
    /// line(s) as a best-effort placement, dimmed, rather than dropped.
    pub detached: bool,
    /// The relocated (or, if `detached`, original) inclusive start line.
    /// Equal to `end` for a single-line comment.
    pub start: u32,
    /// The relocated (or, if `detached`, original) inclusive end line.
    pub end: u32,
}

/// A `(file, 1-based line) -> comments there` lookup, built fresh from
/// [`build_index`] whenever the comment log or the working tree changes.
/// Cheap to hold across frames (the diff pane's render pass does a lookup
/// per visible row, not a scan), and cheap to rebuild wholesale rather than
/// updated incrementally — the comment counts a single review realistically
/// reaches don't justify incremental-update complexity.
///
/// Two maps, not one, because a range comment needs two different answers
/// depending on what's asking: "does a marker belong on this row" (every
/// line the range covers, via [`Self::at`]) versus "does this row start the
/// comment's own body block" (only the first line, via [`Self::starting_at`]
/// — a multi-line range's body must render exactly once, not once per line
/// it spans).
#[derive(Debug, Default)]
pub struct CommentIndex {
    by_location: HashMap<(String, u32), Vec<CommentAnnotation>>,
    // `starting_at` (field and the `Self::starting_at` accessor below) has
    // no production caller yet in #18 — `ui::diff_view`'s body-block
    // rendering still calls `at()` once per row and will keep doing so,
    // rendering a range's body once per covered line, until #19 teaches it
    // to render a range's body only at its first line. Populated and
    // exercised by this module's own tests now so #19 lands on a working,
    // already-tested index rather than needing to design this half of the
    // contract from scratch. Same convention as `Segments::display_len` in
    // `diff::coords`.
    #[allow(dead_code)]
    starting_at: HashMap<(String, u32), Vec<CommentAnnotation>>,
}

impl CommentIndex {
    /// The comments covering (after relocation) `file` (repo-relative,
    /// exactly as diff paths appear) at 1-based `line` — every line a range
    /// spans, not just its start. Empty when there are none, so callers
    /// never need an `Option` just to iterate.
    pub fn at(&self, file: &str, line: u32) -> &[CommentAnnotation] {
        self.by_location
            .get(&(file.to_owned(), line))
            .map_or(&[], Vec::as_slice)
    }

    /// The comments whose relocated (or, if detached, original) *start*
    /// line is `file`:`line` — for a caller that renders a comment's body
    /// once, at the top of the range it annotates, rather than once per
    /// line [`Self::at`] would report it on. See the `starting_at` field's
    /// doc comment on why this has no caller yet.
    #[allow(dead_code)]
    pub fn starting_at(&self, file: &str, line: u32) -> &[CommentAnnotation] {
        self.starting_at
            .get(&(file.to_owned(), line))
            .map_or(&[], Vec::as_slice)
    }
}

/// Builds a [`CommentIndex`] from `comments`, relocating each one against
/// its file's current on-disk content under `repo_root`. Reads each
/// distinct file at most once, regardless of how many comments anchor to
/// it. A file that no longer exists (deleted, or the working tree just
/// doesn't have it — e.g. a comment surviving from a branch switch) leaves
/// every comment on it detached rather than failing the whole build: one
/// unreadable file must not hide every other comment in the diff.
pub fn build_index(repo_root: &Path, comments: &[Comment]) -> CommentIndex {
    let mut by_file: HashMap<&str, Vec<&Comment>> = HashMap::new();
    for comment in comments {
        by_file
            .entry(comment.file.as_str())
            .or_default()
            .push(comment);
    }

    let mut by_location: HashMap<(String, u32), Vec<CommentAnnotation>> = HashMap::new();
    let mut starting_at: HashMap<(String, u32), Vec<CommentAnnotation>> = HashMap::new();
    for (file, file_comments) in by_file {
        let content = std::fs::read_to_string(repo_root.join(file)).ok();
        let lines: Vec<&str> = content
            .as_deref()
            .map(|s| s.lines().collect())
            .unwrap_or_default();

        for comment in file_comments {
            let relocated = relocate_range(comment, &lines);
            let annotation = CommentAnnotation {
                id: comment.id.clone(),
                status: comment.status,
                body: comment.body.clone(),
                detached: relocated.detached,
                start: relocated.start,
                end: relocated.end,
            };

            starting_at
                .entry((file.to_owned(), relocated.start))
                .or_default()
                .push(annotation.clone());

            // Cap how far the marker fan-out goes — see
            // `MAX_RANGE_MARKER_LINES`'s docs. `relocate_range` guarantees
            // `start <= end` whenever `detached` is `false`; a *detached*
            // range keeps its original stored anchors verbatim, which a
            // hand-edited (never CLI-validated) record could have already
            // inverted — guard rather than subtract unchecked, so that case
            // produces no markers at all instead of panicking or wrapping.
            if relocated.end >= relocated.start {
                let span = relocated.end - relocated.start;
                let capped_end =
                    relocated.start + span.min(MAX_RANGE_MARKER_LINES.saturating_sub(1));
                for line in relocated.start..=capped_end {
                    by_location
                        .entry((file.to_owned(), line))
                        .or_default()
                        .push(annotation.clone());
                }
            }
        }
    }

    CommentIndex {
        by_location,
        starting_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::anchor_for;

    fn comment(id: &str, file: &str, lines: &[&str], line: u32, status: Status) -> Comment {
        Comment {
            id: id.to_owned(),
            created_at: 0,
            file: file.to_owned(),
            anchor: anchor_for(lines, line).unwrap(),
            end_anchor: None,
            body: "look at this".to_owned(),
            status,
            resolved_at: None,
        }
    }

    /// As [`comment`], but a range from `start` to `end` (inclusive), both
    /// anchored against `lines` at comment-creation time.
    fn comment_range(
        id: &str,
        file: &str,
        lines: &[&str],
        start: u32,
        end: u32,
        status: Status,
    ) -> Comment {
        Comment {
            end_anchor: Some(anchor_for(lines, end).unwrap()),
            ..comment(id, file, lines, start, status)
        }
    }

    #[test]
    fn build_index_places_an_unchanged_comment_at_its_anchored_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "one\ntwo\nthree\n").unwrap();
        let lines = ["one", "two", "three"];
        let c = comment("id1", "a.rs", &lines, 2, Status::Open);

        let index = build_index(dir.path(), std::slice::from_ref(&c));
        let hits = index.at("a.rs", 2);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "id1");
        assert!(!hits[0].detached);
    }

    #[test]
    fn build_index_marks_a_comment_detached_when_its_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let lines = ["one", "two"];
        let c = comment("id2", "missing.rs", &lines, 1, Status::Open);

        let index = build_index(dir.path(), std::slice::from_ref(&c));
        let hits = index.at("missing.rs", 1);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].detached);
    }

    #[test]
    fn build_index_relocates_a_moved_line_and_indexes_at_the_new_position() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "zero\none\ntwo\nthree\n").unwrap();
        let before = ["one", "two", "three"];
        let c = comment("id3", "a.rs", &before, 1, Status::Open); // anchored to "one"

        let index = build_index(dir.path(), std::slice::from_ref(&c));
        assert!(
            index.at("a.rs", 1).is_empty(),
            "\"one\" is no longer at line 1"
        );
        let hits = index.at("a.rs", 2);
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].detached);
    }

    #[test]
    fn at_returns_an_empty_slice_for_an_unannotated_location() {
        let index = CommentIndex::default();
        assert!(index.at("nowhere.rs", 1).is_empty());
    }

    #[test]
    fn at_marks_every_line_a_range_covers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "one\ntwo\nthree\nfour\n").unwrap();
        let lines = ["one", "two", "three", "four"];
        let c = comment_range("id4", "a.rs", &lines, 2, 3, Status::Open); // "two"..="three"

        let index = build_index(dir.path(), std::slice::from_ref(&c));
        for line in [2, 3] {
            let hits = index.at("a.rs", line);
            assert_eq!(hits.len(), 1, "line {line} should carry a marker");
            assert_eq!(hits[0].start, 2);
            assert_eq!(hits[0].end, 3);
        }
        assert!(index.at("a.rs", 1).is_empty());
        assert!(index.at("a.rs", 4).is_empty());
    }

    #[test]
    fn starting_at_returns_the_comment_only_at_its_first_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "one\ntwo\nthree\nfour\n").unwrap();
        let lines = ["one", "two", "three", "four"];
        let c = comment_range("id5", "a.rs", &lines, 2, 3, Status::Open);

        let index = build_index(dir.path(), std::slice::from_ref(&c));
        assert_eq!(index.starting_at("a.rs", 2).len(), 1);
        assert!(
            index.starting_at("a.rs", 3).is_empty(),
            "the body must not render again at the range's last line"
        );
        // But the gutter marker still covers both lines.
        assert_eq!(index.at("a.rs", 3).len(), 1);
    }

    /// A range whose relocated span is longer than `MAX_RANGE_MARKER_LINES`
    /// (simulated here with a corrupted/hand-edited end anchor far past the
    /// start, rather than an actually-500-line fixture file) must still
    /// produce only a bounded number of `at()` marker entries — see
    /// `MAX_RANGE_MARKER_LINES`'s docs on why an untrusted `end_anchor`
    /// can't be trusted to size an allocation.
    #[test]
    fn at_caps_marker_expansion_at_the_documented_bound() {
        let dir = tempfile::tempdir().unwrap();
        let mut content = String::new();
        for n in 0..1000 {
            content.push_str(&format!("line{n}\n"));
        }
        std::fs::write(dir.path().join("a.rs"), &content).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        let c = comment_range("id6", "a.rs", &lines, 1, 1000, Status::Open);

        let index = build_index(dir.path(), std::slice::from_ref(&c));
        assert_eq!(index.at("a.rs", 1).len(), 1, "start line still marked");
        assert_eq!(
            index.at("a.rs", MAX_RANGE_MARKER_LINES).len(),
            1,
            "the last line within the cap is still marked"
        );
        assert!(
            index.at("a.rs", MAX_RANGE_MARKER_LINES + 1).is_empty(),
            "expansion must stop at the documented bound"
        );
        assert!(index.at("a.rs", 1000).is_empty());
    }

    /// A range comment whose endpoints can't both be relocated (here: the
    /// end line is deleted outright) must be indexed at its *original*
    /// stored start/end, not silently dropped or collapsed to one line —
    /// matching `relocate_range`'s "never reorder or shrink" contract.
    #[test]
    fn build_index_places_a_detached_range_at_its_original_start_and_end() {
        let dir = tempfile::tempdir().unwrap();
        let before = ["one", "two", "three", "four"];
        let c = comment_range("id7", "a.rs", &before, 2, 3, Status::Open); // "two"..="three"

        std::fs::write(dir.path().join("a.rs"), "one\ntwo\nfour\n").unwrap(); // "three" gone

        let index = build_index(dir.path(), std::slice::from_ref(&c));
        let hits = index.starting_at("a.rs", 2);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].detached);
        assert_eq!(hits[0].start, 2);
        assert_eq!(hits[0].end, 3);
    }
}
