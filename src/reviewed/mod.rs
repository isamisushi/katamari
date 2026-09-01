//! Reviewed-hunk state: a reviewer's explicit, per-hunk "I looked at this"
//! mark, persisted at `<repo_root>/.katamari/reviewed.jsonl` and rendered by
//! the diff view as a collapsed one-line marker in place of a reviewed
//! hunk's own content (`RenderRow::ReviewedHunk` — see
//! `crate::diff::model::collapse_reviewed_hunks`). Mirrors
//! `crate::comments` in almost every respect (an append-only JSONL log
//! under the same directory, folded on load); [`store`] owns the file
//! format.
//!
//! The one deliberately different design choice from comments is the
//! identity a mark is keyed on: not a file/line anchor, but
//! [`crate::groups::HunkMeta::id`] — the same content-addressed hash
//! (repo-relative path plus the hunk's *changed* rows only, not its
//! surrounding context or line numbers) `crate::groups::enumerate_hunks`
//! already computes for the semantic-units feature. Reusing it rather than
//! inventing a new hash is what gives a reviewed mark its whole point:
//! surviving an unrelated edit shifting line numbers around it, and
//! resurfacing automatically — as an ordinary unreviewed hunk, with no
//! special "this changed" state to render — the moment the hunk's own
//! content actually changes underneath a previously reviewed mark. A hunk
//! id has no notion of *which* diff it came from (working tree, a
//! branch-vs-base range, a `--pr` fetch), so the same mark is deliberately
//! visible across every scope that resolves to the same repository — by
//! design, not a bug.

pub mod store;

use serde::{Deserialize, Serialize};

/// One hunk marked reviewed — a single line of [`store::ReviewedStore`]'s
/// append-only log. `hunk_id` is `crate::groups::HunkMeta::id`, `path` is
/// [`crate::diff::DiffFile::display_path`] at the moment of marking (kept
/// only for `ktmr reviewed list`'s human-readable output — lookups never
/// key on it, only on `hunk_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewedEntry {
    pub hunk_id: String,
    pub path: String,
    /// Unix seconds — see `crate::comments::now_unix`, reused here rather
    /// than duplicated.
    pub marked_at: u64,
}
