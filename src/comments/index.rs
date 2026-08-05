//! Turns a loaded comment list into something a diff render pass can look up
//! by `(file, line)` in O(1) — the piece that sits between
//! [`super::store::CommentStore::load`] (which knows nothing about a diff's
//! current line numbers) and `ui::diff_view` (which knows nothing about
//! comment relocation). Building the index is where [`super::relocate`]
//! actually runs, once per comment per reload rather than once per row per
//! frame.

use super::{Comment, Status, relocate};
use std::collections::HashMap;
use std::path::Path;

/// One comment's rendering-relevant state at whatever line it currently maps
/// to — everything `ui::diff_view` needs to draw a gutter marker and an
/// inline body block without reaching back into the full [`Comment`] or
/// re-running [`relocate`] itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentAnnotation {
    pub id: String,
    pub status: Status,
    pub body: String,
    /// `true` when [`relocate`] couldn't find the anchored line anywhere in
    /// the current file — this annotation is shown at its *original*
    /// anchor line as a best-effort placement, dimmed, rather than dropped.
    pub detached: bool,
}

/// A `(file, 1-based line) -> comments there` lookup, built fresh from
/// [`build_index`] whenever the comment log or the working tree changes.
/// Cheap to hold across frames (the diff pane's render pass does a lookup
/// per visible row, not a scan), and cheap to rebuild wholesale rather than
/// updated incrementally — the comment counts a single review realistically
/// reaches don't justify incremental-update complexity.
#[derive(Debug, Default)]
pub struct CommentIndex {
    by_location: HashMap<(String, u32), Vec<CommentAnnotation>>,
}

impl CommentIndex {
    /// The comments anchored (after relocation) to `file` (repo-relative,
    /// exactly as diff paths appear) at 1-based `line` — empty when there
    /// are none, so callers never need an `Option` just to iterate.
    pub fn at(&self, file: &str, line: u32) -> &[CommentAnnotation] {
        self.by_location
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
    for (file, file_comments) in by_file {
        let content = std::fs::read_to_string(repo_root.join(file)).ok();
        let lines: Vec<&str> = content
            .as_deref()
            .map(|s| s.lines().collect())
            .unwrap_or_default();

        for comment in file_comments {
            let (line, detached) = match relocate(comment, &lines) {
                Some(line) => (line, false),
                None => (comment.anchor.new_line, true),
            };
            by_location
                .entry((file.to_owned(), line))
                .or_default()
                .push(CommentAnnotation {
                    id: comment.id.clone(),
                    status: comment.status,
                    body: comment.body.clone(),
                    detached,
                });
        }
    }

    CommentIndex { by_location }
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
            body: "look at this".to_owned(),
            status,
            resolved_at: None,
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
}
