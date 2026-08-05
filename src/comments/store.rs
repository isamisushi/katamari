//! The on-disk comment log: an append-only JSONL file at
//! `<repo_root>/.katamari/comments.jsonl`, and the fold that turns its
//! sequence of records into the current state of every comment. Append-only
//! (never rewritten in place) so a concurrent reader — the TUI's live watch,
//! or another `ktmr comments` invocation — never observes a half-written
//! file, and so the whole history stays `tail -f`-friendly for a reviewer or
//! agent who just wants to watch activity scroll by.
//!
//! Two kinds of line appear in the file, both one JSON object per line:
//!
//! - A **creation** record: the full [`Comment`] shape, written once by
//!   [`CommentStore::append_comment`].
//! - An **amendment** record: `{id, status, resolved_at}`, written by
//!   [`CommentStore::append_status`] every time a comment's status changes
//!   (`ktmr comments resolve`/`reopen`, or a future UI action). Rather than
//!   rewriting the comment's own line — which would break both the
//!   append-only property and `tail -f` — a status change is just one more
//!   line appended to the end; [`CommentStore::load`] folds the whole file
//!   by id, and the last amendment for a given id wins.

use super::{Comment, Status};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The filename under `.katamari/` — not configurable, since the whole point
/// is a fixed, predictable location a reviewer's `ktmr diff` session and an
/// agent's `ktmr comments` invocations agree on without any coordination.
const FILE_NAME: &str = "comments.jsonl";

/// A status-change amendment record — see the module docs. Deliberately a
/// separate, smaller type from [`Comment`] rather than `Comment` with most
/// fields defaulted: that would let a malformed or truncated creation record
/// silently masquerade as a valid amendment, which is exactly the ambiguity
/// [`Record`]'s field-driven `untagged` matching depends on *not* happening.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Amendment {
    id: String,
    status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved_at: Option<u64>,
}

/// One line of the JSONL file, matched against whichever variant's required
/// fields it actually has. `Create` is tried first: its required fields
/// (`created_at`, `file`, `anchor`, `body`) are exactly what an `Amendment`
/// record lacks, so a creation record can never be misparsed as an
/// amendment or vice versa — the two shapes don't overlap.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum Record {
    Create(Comment),
    Amend(Amendment),
}

/// Any failure reading or writing the comment log — surfaced to the CLI as a
/// plain error message (see `main.rs`'s `ktmr comments` handlers) and to the
/// TUI as a status-bar note. A hand-rolled `Display`/`Error` pair rather
/// than pulling in a derive-macro crate, matching [`crate::lsp::LspError`]'s
/// precedent elsewhere in this codebase for a small, closed error enum.
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
    NotFound(String),
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
                    "{}:{line}: malformed comment record: {source}",
                    path.display()
                )
            }
            StoreError::NotFound(id) => write!(f, "no comment with id {id:?}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Owns the path to one repository's comment log and every read/write
/// against it. Cheap to construct — holds nothing but a [`PathBuf`] — so
/// callers (the TUI's event loop, each `ktmr comments` CLI invocation) build
/// one wherever they need it rather than threading a shared instance
/// through.
pub struct CommentStore {
    path: PathBuf,
}

impl CommentStore {
    pub fn new(repo_root: &Path) -> Self {
        Self {
            path: repo_root.join(".katamari").join(FILE_NAME),
        }
    }

    /// The JSONL file's path — exposed so the TUI's comments watcher (see
    /// [`crate::watch::spawn_comments_watcher`]) knows what to watch without
    /// duplicating the `.katamari/comments.jsonl` layout decision.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Creates `.katamari/` (a no-op if it already exists) — split out from
    /// the append methods so the comments watcher can call it too, ensuring
    /// there's always a directory to watch even in a repo where no comment
    /// has been written yet.
    ///
    /// Also drops a `.katamari/.gitignore` containing `*`, the first time
    /// the directory is created, so `comments.jsonl` doesn't show up as an
    /// untracked "new file" in the very diff it's annotating — the same
    /// "keep katamari's own bookkeeping out of the review" intent
    /// [`crate::watch`]'s `HARDCODED_EXCLUDES` already applies to the
    /// filesystem watcher. A reviewer who *wants* to commit their comment
    /// log (e.g. to keep it alongside the branch) can simply delete that
    /// file; this only sets the default.
    pub fn ensure_dir(&self) -> Result<(), StoreError> {
        let dir = self.path.parent().expect("path always has a parent");
        let is_new = !dir.exists();
        fs::create_dir_all(dir).map_err(|source| StoreError::Write {
            path: dir.to_path_buf(),
            source,
        })?;
        if is_new {
            let gitignore = dir.join(".gitignore");
            // Best-effort: a failure here shouldn't block writing the
            // comment itself, which is what the caller actually asked for.
            let _ = fs::write(&gitignore, "*\n");
        }
        Ok(())
    }

    /// Reads every record, folds status amendments onto their creation
    /// records by id (last amendment wins — see the module docs), and
    /// returns the resulting comments in file order (i.e. creation order,
    /// unaffected by how many times a comment's status later changed). A
    /// comment log that doesn't exist yet reads as empty rather than an
    /// error — the ordinary state of a repo nobody has commented on.
    ///
    /// Tolerates exactly one kind of corruption: a final line that isn't
    /// valid JSON *and* isn't newline-terminated — the signature of a writer
    /// that crashed or was killed mid-`write()`. Any other malformed line
    /// (mid-file, or a final line that *is* newline-terminated but still
    /// doesn't parse) is treated as real corruption and reported as an
    /// error rather than silently dropped, since silently losing an
    /// established comment is worse than surfacing the problem.
    pub fn load(&self) -> Result<Vec<Comment>, StoreError> {
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
        let mut by_id: HashMap<String, Comment> = HashMap::new();

        for (idx, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Record>(line) {
                Ok(Record::Create(comment)) => {
                    if !by_id.contains_key(&comment.id) {
                        order.push(comment.id.clone());
                    }
                    by_id.insert(comment.id.clone(), comment);
                }
                Ok(Record::Amend(amendment)) => {
                    if let Some(comment) = by_id.get_mut(&amendment.id) {
                        comment.status = amendment.status;
                        comment.resolved_at = amendment.resolved_at;
                    }
                    // An amendment for an id whose creation record hasn't
                    // been seen (shouldn't happen in a file this store
                    // wrote itself, but a hand-edited or concatenated file
                    // could produce one) has nothing to amend — dropped
                    // rather than treated as an error, since the file as a
                    // whole is still well-formed JSONL.
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

    /// Appends a creation record for `comment` — the only way a new comment
    /// enters the log. Creates `.katamari/` on first use.
    pub fn append_comment(&self, comment: &Comment) -> Result<(), StoreError> {
        self.append_record(&Record::Create(comment.clone()))
    }

    /// Appends a status-change amendment for `id` — see the module docs on
    /// why this is a new line rather than editing the comment's original
    /// one. Does not check that `id` already exists in the log; callers
    /// that need "does this comment exist" (the CLI's `resolve`/`reopen`,
    /// which should fail loudly on a typo'd id) check via [`Self::load`]
    /// first.
    pub fn append_status(
        &self,
        id: &str,
        status: Status,
        resolved_at: Option<u64>,
    ) -> Result<(), StoreError> {
        self.append_record(&Record::Amend(Amendment {
            id: id.to_owned(),
            status,
            resolved_at,
        }))
    }

    /// As [`Self::append_status`], but fails with [`StoreError::NotFound`]
    /// rather than silently appending an orphan amendment when `id` isn't
    /// among the currently loaded comments — what `ktmr comments
    /// resolve`/`reopen` want: a typo'd id should be reported, not accepted
    /// and forgotten.
    pub fn set_status(
        &self,
        id: &str,
        status: Status,
        resolved_at: Option<u64>,
    ) -> Result<(), StoreError> {
        let exists = self.load()?.iter().any(|c| c.id == id);
        if !exists {
            return Err(StoreError::NotFound(id.to_owned()));
        }
        self.append_status(id, status, resolved_at)
    }

    fn append_record(&self, record: &Record) -> Result<(), StoreError> {
        self.ensure_dir()?;
        let mut line = serde_json::to_string(record).expect("Record always serializes");
        line.push('\n');
        // `OpenOptions::append` maps to `O_APPEND`, which the OS guarantees
        // atomically relocates the write to the file's current end even
        // against a concurrent writer (another `ktmr comments` process, or
        // this same TUI session's own compose overlay) — the property that
        // makes a plain-file, no-daemon design safe to write from more than
        // one process at once without a lock file.
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| StoreError::Write {
                path: self.path.clone(),
                source,
            })?;
        file.write_all(line.as_bytes())
            .map_err(|source| StoreError::Write {
                path: self.path.clone(),
                source,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::{anchor_for, now_unix};

    fn sample_comment(id: &str, status: Status) -> Comment {
        let lines = ["fn main() {}"];
        Comment {
            id: id.to_owned(),
            created_at: now_unix(),
            file: "src/main.rs".to_owned(),
            anchor: anchor_for(&lines, 1).unwrap(),
            body: "please rename this".to_owned(),
            status,
            resolved_at: None,
        }
    }

    #[test]
    fn loading_a_missing_file_returns_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = CommentStore::new(dir.path());
        assert_eq!(store.load().unwrap(), Vec::new());
    }

    #[test]
    fn creating_the_directory_drops_a_gitignore_so_the_log_never_pollutes_a_reviewed_diff() {
        let dir = tempfile::tempdir().unwrap();
        let store = CommentStore::new(dir.path());
        store
            .append_comment(&sample_comment("gggg7777", Status::Open))
            .unwrap();

        let gitignore = dir.path().join(".katamari").join(".gitignore");
        assert_eq!(fs::read_to_string(gitignore).unwrap(), "*\n");
    }

    #[test]
    fn append_then_load_round_trips_a_comment() {
        let dir = tempfile::tempdir().unwrap();
        let store = CommentStore::new(dir.path());
        let comment = sample_comment("aaaa1111", Status::Open);
        store.append_comment(&comment).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded, vec![comment]);
        assert!(store.path().exists());
    }

    #[test]
    fn a_later_amendment_wins_over_an_earlier_one_and_the_original_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = CommentStore::new(dir.path());
        let comment = sample_comment("bbbb2222", Status::Open);
        store.append_comment(&comment).unwrap();
        store
            .append_status("bbbb2222", Status::Resolved, Some(100))
            .unwrap();
        store.append_status("bbbb2222", Status::Open, None).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].status, Status::Open);
        assert_eq!(loaded[0].resolved_at, None);
    }

    #[test]
    fn load_preserves_creation_order_regardless_of_amendment_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = CommentStore::new(dir.path());
        store
            .append_comment(&sample_comment("id-1", Status::Open))
            .unwrap();
        store
            .append_comment(&sample_comment("id-2", Status::Open))
            .unwrap();
        store
            .append_status("id-1", Status::Resolved, Some(1))
            .unwrap();

        let loaded = store.load().unwrap();
        let ids: Vec<&str> = loaded.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["id-1", "id-2"]);
    }

    #[test]
    fn an_amendment_for_an_unknown_id_is_dropped_without_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let store = CommentStore::new(dir.path());
        store
            .append_status("never-created", Status::Resolved, Some(1))
            .unwrap();
        assert_eq!(store.load().unwrap(), Vec::new());
    }

    /// A writer killed mid-`write()` leaves a final line that isn't valid
    /// JSON and has no trailing newline — this must be silently dropped,
    /// not treated as file corruption, so a reader racing an in-progress
    /// append never sees an error.
    #[test]
    fn a_truncated_final_line_without_a_trailing_newline_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let store = CommentStore::new(dir.path());
        let comment = sample_comment("cccc3333", Status::Open);
        store.append_comment(&comment).unwrap();

        // Simulate a second, in-flight write that got cut off partway
        // through the JSON object, with no trailing newline yet.
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(store.path())
            .unwrap();
        write!(file, "{{\"id\": \"dddd4444\", \"created_at\": 1, \"fi").unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded, vec![comment], "the truncated line must be ignored");
    }

    /// A malformed line that *does* end in a newline (i.e. was fully
    /// flushed) is real corruption, not an in-flight write — this must
    /// still surface as an error rather than being silently dropped.
    #[test]
    fn a_fully_written_but_malformed_line_is_a_real_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = CommentStore::new(dir.path());
        store.ensure_dir().unwrap();
        fs::write(store.path(), "{not even close to json}\n").unwrap();

        assert!(matches!(store.load(), Err(StoreError::Parse { .. })));
    }

    #[test]
    fn set_status_rejects_an_unknown_id_without_writing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let store = CommentStore::new(dir.path());
        let result = store.set_status("nope", Status::Resolved, Some(1));
        assert!(matches!(result, Err(StoreError::NotFound(id)) if id == "nope"));
        assert!(
            !store.path().exists(),
            "a rejected set_status must not create the file"
        );
    }

    #[test]
    fn blank_lines_between_records_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = CommentStore::new(dir.path());
        store.ensure_dir().unwrap();
        let comment = sample_comment("eeee5555", Status::Open);
        let json = serde_json::to_string(&comment).unwrap();
        fs::write(store.path(), format!("{json}\n\n\n")).unwrap();

        assert_eq!(store.load().unwrap(), vec![comment]);
    }
}
