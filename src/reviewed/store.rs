//! Persistence for reviewed-hunk marks: an append-only JSONL file at
//! `<repo_root>/.katamari/reviewed.jsonl`, one record per line, mirroring
//! `crate::comments::store` almost exactly (see that module's docs for the
//! append-only/`O_APPEND`/fold-on-load reasoning, which applies here
//! unchanged). Two record shapes, told apart by which required fields they
//! carry (the same untagged-with-disjoint-required-fields trick
//! `comments::store::Record` uses):
//!
//! - A **mark**: the full [`ReviewedEntry`] shape (`hunk_id`, `path`,
//!   `marked_at`), written once per `r`/`R`/bulk-mark keypress.
//! - An **unmark**: `{hunk_id, unmarked_at}` — no `path`, so it can never be
//!   mistaken for a mark record. Unlike a comment's status amendment,
//!   unmarking needs no tombstone value: presence in the folded map *is*
//!   the "reviewed" state, so an unmark simply removes the entry rather
//!   than recording a new status.
//!
//! Unlike `comments.jsonl`/`groups.jsonl`, nothing ever compacts this file
//! — see the katamari-review-state design notes: a stale hunk id (the path
//! was rewritten, or a fold merge changed a hunk's identity) is just a few
//! inert bytes nothing looks up again, the same cost class this codebase
//! already accepts for orphaned comment amendments. [`ReviewedStore::clear`]
//! is the only reset, and — because this file has exactly one writer in
//! ordinary use (the interactive TUI session; there is deliberately no CLI
//! mutator besides `clear`, see `main.rs`'s `ReviewedCommand`) — it's
//! allowed to just delete the file outright rather than append a tombstone
//! the way comments' resolve/reopen do.

use super::ReviewedEntry;
use crate::comments::CommentStore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "reviewed.jsonl";

/// An unmark amendment record — see the module docs. A separate, smaller
/// type from [`ReviewedEntry`] rather than that type with fields defaulted,
/// for the same reason `comments::store::Amendment` is separate from
/// `Comment`: it keeps the two record shapes' required-field sets from ever
/// overlapping, which is what makes [`Record`]'s untagged dispatch
/// unambiguous.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Unmark {
    hunk_id: String,
    unmarked_at: u64,
}

/// One line of the JSONL file. `Mark` is tried first: its required fields
/// (`path`, `marked_at`) are exactly what an `Unmark` record lacks, so the
/// two shapes can never be confused for each other regardless of dispatch
/// order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum Record {
    Mark(ReviewedEntry),
    Unmark(Unmark),
}

/// Any failure reading or writing the reviewed log — mirrors
/// `comments::store::StoreError`'s shape, minus the `NotFound` variant that
/// store needs for `set_status`'s "reject an unknown id" behavior:
/// `ReviewedStore` has no CLI mutator that looks an id up before writing
/// (marking is TUI-only and always succeeds against whatever hunk the
/// cursor is on), so there's no call site that would ever construct one.
#[derive(Debug)]
pub enum StoreError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Read { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            StoreError::Write { path, source } => {
                write!(f, "failed to write {}: {source}", path.display())
            }
            StoreError::Parse { path, line, source } => {
                write!(
                    f,
                    "{}:{line}: malformed reviewed record: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for StoreError {}

/// Owns the path to one repository's reviewed-hunk log and every read/write
/// against it. Cheap to construct, like `CommentStore`/`GroupStore` — holds
/// only a [`PathBuf`] plus a [`CommentStore`] used solely for its
/// `.katamari/` directory (and self-ignoring `.gitignore`) creation logic,
/// the same delegation `GroupStore` uses so that layout decision lives in
/// exactly one place.
pub struct ReviewedStore {
    path: PathBuf,
    comments: CommentStore,
}

impl ReviewedStore {
    pub fn new(repo_root: &Path) -> Self {
        Self {
            path: repo_root.join(".katamari").join(FILE_NAME),
            comments: CommentStore::new(repo_root),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Creates `.katamari/` (and its self-ignoring `.gitignore`) via the
    /// held [`CommentStore`] — see that method's own docs for why the
    /// `.gitignore` exists at all.
    fn ensure_dir(&self) -> Result<(), StoreError> {
        self.comments.ensure_dir().map_err(|e| StoreError::Write {
            path: self.path.clone(),
            source: io::Error::other(e.to_string()),
        })
    }

    /// Reads every record and folds unmark amendments onto their mark
    /// records by `hunk_id` (a mark inserts/overwrites; an unmark removes
    /// the entry outright — presence *is* the reviewed state, so there's no
    /// status field to flip the way `comments::store::load` flips one).
    /// Order in the returned `Vec` is first-mark order, matching
    /// `CommentStore::load`'s "creation order regardless of later
    /// amendments" contract as closely as a presence/absence model allows.
    ///
    /// A missing file loads as empty (never marked anything yet, the
    /// ordinary state). Tolerates exactly the same "truncated final line
    /// with no trailing newline" corruption `CommentStore::load` does —
    /// see that method's docs for why that specific case is safe to drop
    /// silently rather than treat as real corruption.
    pub fn load(&self) -> Result<Vec<ReviewedEntry>, StoreError> {
        let content = match fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(StoreError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };

        let physically_truncated = !content.is_empty() && !content.ends_with('\n');
        let lines: Vec<&str> = content.lines().collect();
        let last_idx = lines.len().saturating_sub(1);

        let mut order: Vec<String> = Vec::new();
        let mut by_id: HashMap<String, ReviewedEntry> = HashMap::new();

        for (idx, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Record>(line) {
                Ok(Record::Mark(entry)) => {
                    if !by_id.contains_key(&entry.hunk_id) {
                        order.push(entry.hunk_id.clone());
                    }
                    by_id.insert(entry.hunk_id.clone(), entry);
                }
                Ok(Record::Unmark(unmark)) => {
                    by_id.remove(&unmark.hunk_id);
                }
                Err(_) if idx == last_idx && physically_truncated => {
                    // Tolerated — see this method's docs.
                }
                Err(source) => {
                    return Err(StoreError::Parse {
                        path: self.path.clone(),
                        line: idx + 1,
                        source,
                    });
                }
            }
        }

        Ok(order
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .collect())
    }

    /// Appends one mark record for `id`/`path` — a single `r`/`R` keypress
    /// on one hunk.
    pub fn append_mark(&self, id: &str, path: &str) -> Result<(), StoreError> {
        self.append_record(&Record::Mark(ReviewedEntry {
            hunk_id: id.to_owned(),
            path: path.to_owned(),
            marked_at: crate::comments::now_unix(),
        }))
    }

    pub fn append_unmark(&self, id: &str) -> Result<(), StoreError> {
        self.append_record(&Record::Unmark(Unmark {
            hunk_id: id.to_owned(),
            unmarked_at: crate::comments::now_unix(),
        }))
    }

    /// Appends a mark record for every `(hunk_id, path)` pair in `entries`
    /// through one file open — the bulk "mark whole file"/"mark all
    /// visible" path, so marking a hundred hunks at once doesn't pay a
    /// hundred separate `open()` calls. A no-op (no file touched at all,
    /// not even to create `.katamari/`) for an empty slice.
    pub fn append_marks(&self, entries: &[(String, String)]) -> Result<(), StoreError> {
        if entries.is_empty() {
            return Ok(());
        }
        self.ensure_dir()?;
        let marked_at = crate::comments::now_unix();
        let mut buf = String::new();
        for (hunk_id, path) in entries {
            let record = Record::Mark(ReviewedEntry {
                hunk_id: hunk_id.clone(),
                path: path.clone(),
                marked_at,
            });
            buf.push_str(&serde_json::to_string(&record).expect("Record always serializes"));
            buf.push('\n');
        }
        self.write_append(&buf)
    }

    fn append_record(&self, record: &Record) -> Result<(), StoreError> {
        self.ensure_dir()?;
        let mut line = serde_json::to_string(record).expect("Record always serializes");
        line.push('\n');
        self.write_append(&line)
    }

    fn write_append(&self, content: &str) -> Result<(), StoreError> {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| StoreError::Write {
                path: self.path.clone(),
                source,
            })?;
        file.write_all(content.as_bytes())
            .map_err(|source| StoreError::Write {
                path: self.path.clone(),
                source,
            })
    }

    /// Removes the reviewed log outright — `ktmr reviewed clear`'s only
    /// job. A missing file is a no-op, not an error (there was nothing to
    /// clear). See the module docs for why this file, uniquely among
    /// katamari's JSONL logs, is allowed a destructive reset instead of an
    /// append-only tombstone: it has no CLI mutator to race against.
    pub fn clear(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_a_missing_file_returns_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReviewedStore::new(dir.path());
        assert_eq!(store.load().unwrap(), Vec::new());
    }

    #[test]
    fn mark_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReviewedStore::new(dir.path());
        store.append_mark("aaaa1111", "src/a.rs").unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].hunk_id, "aaaa1111");
        assert_eq!(loaded[0].path, "src/a.rs");
    }

    #[test]
    fn unmark_removes_the_entry_from_the_fold() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReviewedStore::new(dir.path());
        store.append_mark("aaaa1111", "src/a.rs").unwrap();
        store.append_unmark("aaaa1111").unwrap();

        assert_eq!(store.load().unwrap(), Vec::new());
    }

    #[test]
    fn remarking_after_an_unmark_reappears() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReviewedStore::new(dir.path());
        store.append_mark("aaaa1111", "src/a.rs").unwrap();
        store.append_unmark("aaaa1111").unwrap();
        store.append_mark("aaaa1111", "src/a.rs").unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].hunk_id, "aaaa1111");
    }

    #[test]
    fn append_marks_writes_every_pair_through_one_open() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReviewedStore::new(dir.path());
        store
            .append_marks(&[
                ("id1".to_owned(), "a.rs".to_owned()),
                ("id2".to_owned(), "b.rs".to_owned()),
            ])
            .unwrap();

        let loaded = store.load().unwrap();
        let ids: Vec<&str> = loaded.iter().map(|e| e.hunk_id.as_str()).collect();
        assert_eq!(ids, vec!["id1", "id2"]);
    }

    #[test]
    fn append_marks_on_an_empty_slice_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReviewedStore::new(dir.path());
        store.append_marks(&[]).unwrap();
        assert!(!store.path().exists());
    }

    #[test]
    fn creating_the_directory_drops_a_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReviewedStore::new(dir.path());
        store.append_mark("aaaa1111", "src/a.rs").unwrap();

        let gitignore = dir.path().join(".katamari").join(".gitignore");
        assert_eq!(fs::read_to_string(gitignore).unwrap(), "*\n");
    }

    /// As `comments::store`'s equivalent test: a writer killed mid-`write()`
    /// leaves a final line that isn't valid JSON and has no trailing
    /// newline — tolerated, not treated as corruption.
    #[test]
    fn a_truncated_final_line_without_a_trailing_newline_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReviewedStore::new(dir.path());
        store.append_mark("aaaa1111", "src/a.rs").unwrap();

        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(store.path())
            .unwrap();
        write!(file, "{{\"hunk_id\": \"bbbb2222\", \"pa").unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].hunk_id, "aaaa1111");
    }

    #[test]
    fn a_fully_written_but_malformed_line_is_a_real_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReviewedStore::new(dir.path());
        store.append_mark("aaaa1111", "src/a.rs").unwrap();
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(store.path())
            .unwrap();
        writeln!(file, "{{not even close to json}}").unwrap();

        assert!(matches!(store.load(), Err(StoreError::Parse { .. })));
    }

    #[test]
    fn clear_on_a_missing_file_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReviewedStore::new(dir.path());
        store.clear().unwrap();
        assert!(!store.path().exists());
    }

    #[test]
    fn clear_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReviewedStore::new(dir.path());
        store.append_mark("aaaa1111", "src/a.rs").unwrap();
        assert!(store.path().exists());
        store.clear().unwrap();
        assert!(!store.path().exists());
    }
}
