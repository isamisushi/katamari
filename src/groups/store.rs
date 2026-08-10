//! Persistence for groupings: an append-only JSONL file at
//! `<repo_root>/.katamari/groups.jsonl`, one [`Grouping`] per line, newest
//! appended last. The same append-only design as
//! [`crate::comments::CommentStore`] and for the same reasons — a
//! concurrent reader never sees a half-written file, and `O_APPEND` makes
//! multi-process writes safe without a lock. Lookup folds by `diff_key`
//! with the last record winning, so re-grouping the same diff (the user
//! asked for a fresh take, or a different agent CLI answered) is just one
//! more appended line, and history is never destroyed.

use super::Grouping;
use crate::comments::CommentStore;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "groups.jsonl";

pub struct GroupStore {
    path: PathBuf,
    /// Held only for [`CommentStore::ensure_dir`] — the `.katamari/`
    /// directory plus its self-ignoring `.gitignore` is that store's
    /// concern, and duplicating the layout decision here would let the two
    /// drift.
    comments: CommentStore,
}

impl GroupStore {
    pub fn new(repo_root: &Path) -> Self {
        Self {
            path: repo_root.join(".katamari").join(FILE_NAME),
            comments: CommentStore::new(repo_root),
        }
    }

    /// The newest grouping recorded for `diff_key`, or `None` — a missing
    /// file is the ordinary "never grouped anything here" state, not an
    /// error. Malformed lines are skipped rather than fatal: unlike a
    /// comment log, a grouping is a regenerable cache, so losing one line
    /// costs a re-run, while refusing to load costs the whole feature.
    pub fn load_for(&self, diff_key: &str) -> io::Result<Option<Grouping>> {
        let content = match fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut found = None;
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(grouping) = serde_json::from_str::<Grouping>(line)
                && grouping.diff_key == diff_key
            {
                found = Some(grouping);
            }
        }
        Ok(found)
    }

    pub fn append(&self, grouping: &Grouping) -> io::Result<()> {
        self.comments
            .ensure_dir()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let mut line = serde_json::to_string(grouping).expect("Grouping always serializes");
        line.push('\n');
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groups::{Unit, UnitKind};

    fn sample(diff_key: &str, label: &str) -> Grouping {
        Grouping {
            diff_key: diff_key.to_owned(),
            agent: "claude".to_owned(),
            created_at: 1,
            units: vec![Unit {
                label: label.to_owned(),
                description: String::new(),
                hunk_ids: vec!["aa".into()],
                kind: UnitKind::Concern,
            }],
        }
    }

    #[test]
    fn missing_file_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        assert_eq!(store.load_for("k1").unwrap(), None);
    }

    #[test]
    fn last_grouping_for_a_key_wins() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        store.append(&sample("k1", "first")).unwrap();
        store.append(&sample("k2", "other-key")).unwrap();
        store.append(&sample("k1", "second")).unwrap();

        let loaded = store.load_for("k1").unwrap().unwrap();
        assert_eq!(loaded.units[0].label, "second");
    }

    #[test]
    fn a_malformed_line_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        store.append(&sample("k1", "good")).unwrap();
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join(".katamari").join(FILE_NAME))
            .unwrap();
        writeln!(file, "{{truncated garbage").unwrap();

        assert!(store.load_for("k1").unwrap().is_some());
    }

    #[test]
    fn append_creates_the_self_ignoring_katamari_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        store.append(&sample("k1", "x")).unwrap();
        let gitignore = dir.path().join(".katamari").join(".gitignore");
        assert_eq!(fs::read_to_string(gitignore).unwrap(), "*\n");
    }
}
