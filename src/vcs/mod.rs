//! Abstracts over version-control backends so `diff::model` and the UI never
//! call `git` directly. M1 ships one implementation ([`git::GitSource`]);
//! later milestones can add a `jj` backend behind the same trait without
//! touching anything upstream of it.

pub mod git;
pub mod jj;

use anyhow::Result;
use git::GitSource;
use jj::JjRepo;
use std::path::{Path, PathBuf};

/// A source of diff text for the review UI to render.
pub trait DiffSource {
    /// Unified diff text for the working tree against the review baseline
    /// (HEAD, or the empty tree when the repo has no commits yet), including
    /// files that exist on disk but aren't tracked by the VCS yet, rendered
    /// as all-add pseudo-diffs.
    fn working_tree_diff(&self) -> Result<String>;

    /// Unified diff text for staged changes (the index) against the review
    /// baseline. Unlike [`Self::working_tree_diff`], this never includes
    /// untracked files — they aren't in the index.
    fn staged_diff(&self) -> Result<String>;

    /// Unified diff text for a revision or range, e.g. `HEAD~2` (that
    /// commit's own changes against its parent) or `main..feature` (passed
    /// through to the VCS as a range).
    fn range_diff(&self, range: &str) -> Result<String>;

    /// Absolute path to the repository root, used for display in the status
    /// bar.
    fn repo_root(&self) -> Result<PathBuf>;
}

/// One row of `ktmr log`'s browsable history — either a real commit/change,
/// or (git-only) the synthetic "local changes" row standing in for the dirty
/// working tree (a jj repo never needs this: the working copy is already a
/// real, listed change — see [`jj::JjRepo::log`]'s docs). Shared by
/// [`git::GitSource::log`] and [`jj::JjRepo::log`] so
/// [`crate::ui::log_view::LogView`] can render either backend through one
/// shape rather than two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEntry {
    Revision(RevisionEntry),
    /// `changed_files` is the count `git status`-equivalent plumbing found
    /// (tracked changes plus untracked files) — cheap to compute (see
    /// [`git::GitSource::log`]'s docs) alongside the check for whether this
    /// row exists at all, so it's carried along rather than recomputed if a
    /// caller wants to display it.
    LocalChanges {
        changed_files: usize,
    },
}

/// A single commit (git) or change (jj), normalized to the fields
/// [`crate::ui::log_view::LogView`] needs to render a row and to diff it —
/// against a parent (single-revision) or another entry (range). `id` is
/// always the full, unabbreviated identifier (a git commit's full SHA, a
/// jj change id's full 32-character form) — the same "never hold onto an
/// abbreviation that can grow ambiguous later" rule [`jj::SnapshotOp::op_id`]
/// documents, since `id` is what a later diff call actually passes to
/// git/jj, while `short_id` exists purely for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionEntry {
    pub id: String,
    pub short_id: String,
    /// First line of the commit message / change description.
    pub summary: String,
    pub author: String,
    pub time_unix: i64,
    /// Branch/bookmark/tag names pointing at this revision, in whatever
    /// order git/jj reported them — empty when none do.
    pub refs: Vec<String>,
    /// git: always `false` (a git commit is never "the working copy").
    /// jj: `true` for `@` — jj's own marker for which change *is* the
    /// working copy, carried through as data rather than a graph glyph so
    /// [`crate::ui::log_view::LogView`] can style it without re-parsing
    /// anything.
    pub is_working_copy: bool,
}

/// Which backend [`crate::ui::log_view::LogView`] (and `ktmr log --dump`)
/// queries for history — resolved once, the same "detect once, reuse for
/// every query in the session" shape [`jj::JjRepo`] and [`git::GitSource`]
/// already follow individually. Picking jj over git whenever both are
/// available mirrors `ui::mod::detect_jj_repo`'s preference: a colocated jj
/// repo's own history (changes, including the working copy as a real entry)
/// is the more useful one to browse.
pub enum LogBackend {
    Jj(JjRepo),
    Git(GitSource),
}

impl LogBackend {
    /// Resolves the backend for `repo_root`: a colocated jj repo (`.jj` next
    /// to `.git`, with `jj` on `PATH`) if detected, otherwise plain git —
    /// which always works, since every repository `ktmr` operates on is a
    /// git repository first (see `GitSource::discover`'s callers).
    pub fn detect(repo_root: &Path) -> Self {
        match jj::resolve_jj_bin().and_then(|bin| JjRepo::detect(repo_root, bin)) {
            Some(repo) => LogBackend::Jj(repo),
            None => LogBackend::Git(GitSource::at(repo_root.to_owned())),
        }
    }

    pub fn repo_root(&self) -> &Path {
        match self {
            LogBackend::Jj(repo) => repo.repo_root(),
            LogBackend::Git(git) => git.repo_root_path(),
        }
    }

    pub fn log(&self, limit: usize) -> Result<Vec<LogEntry>> {
        match self {
            LogBackend::Jj(repo) => repo.log(limit),
            LogBackend::Git(git) => git.log(limit),
        }
    }

    /// The diff for one revision against its parent — `entry` must be a
    /// [`LogEntry::Revision`] (see [`Self::working_tree_diff`] for the
    /// git-only [`LogEntry::LocalChanges`] row's own diff).
    pub fn revision_diff(&self, entry: &RevisionEntry) -> Result<String> {
        match self {
            LogBackend::Jj(repo) => repo.revision_diff(&entry.id),
            // git's existing single-revision machinery (`range_diff` with a
            // plain, non-range revspec) already means exactly this — see
            // `git::plan_range`.
            LogBackend::Git(git) => git.range_diff(&entry.id),
        }
    }

    /// The diff between two revisions, `from` (older) and `to` (newer) —
    /// [`crate::ui::log_view::LogView`]'s 2-point range selection.
    pub fn range_diff(&self, from: &RevisionEntry, to: &RevisionEntry) -> Result<String> {
        match self {
            LogBackend::Jj(repo) => repo.revision_range_diff(Some(&from.id), Some(&to.id)),
            LogBackend::Git(git) => git.range_diff(&format!("{}..{}", from.id, to.id)),
        }
    }

    /// The working tree's own diff — only ever called for a
    /// [`LogEntry::LocalChanges`] row, which [`jj::JjRepo::log`] never
    /// produces (see that row's docs on why jj doesn't need a synthetic
    /// entry), so this has nothing to do for a jj-backed session.
    pub fn working_tree_diff(&self) -> Result<String> {
        match self {
            LogBackend::Jj(_) => unreachable!(
                "LocalChanges is a git-only row; jj's log never produces one to confirm"
            ),
            LogBackend::Git(git) => git.working_tree_diff(),
        }
    }
}
