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

/// Issue #8: whether `rev` is the kind of revision text that can point at a
/// *different* commit tomorrow than it does today — `HEAD`, a branch name, a
/// jj change id (`@`, `@-`, `main`, `ruwywzxw...`) — as opposed to an
/// already-fixed object id (a full or abbreviated commit hash). Feeds
/// `ui::mod`'s decision to watch a historical scope (`ktmr diff -r HEAD`,
/// the scope menu's "Revision…") for amends at all: a scope this returns
/// `false` for never seeds a `MovingScopeState`, so it's simply never
/// re-diffed, no matter how many times the underlying ref file changes.
///
/// Two deliberate V1 boundaries (not oversights — see issue #8's acceptance
/// criteria, which only ever describe a single revision, not a range):
/// - A `..`/`...` range is always classified immutable, even though its
///   individual endpoints can each be moving — live-refreshing a two-sided
///   range is out of scope for the first cut of this feature.
/// - [`looks_like_object_id`] is a conservative, purely lexical guess: a
///   branch/bookmark name that happens to be 4-40 hex characters (`git
///   branch dead`, `jj bookmark create face` are both legal) is
///   misclassified as immutable. That's a false negative — a genuinely
///   moving revision silently never refreshed — which is the safer failure
///   mode here: nothing about a raw-looking hash makes a reviewer expect it
///   to move, so the rare misclassification just costs one scope that never
///   auto-refreshes, never a scope that spuriously re-diffs on watcher
///   noise.
pub fn is_moving_revision(rev: &str) -> bool {
    !rev.contains("..") && !looks_like_object_id(rev)
}

/// A conservative "this looks like a git object id" check: 4 to 40
/// characters (git's shortest unambiguous abbreviation through a full
/// SHA-1 — a SHA-256 repository's 64-character hashes fall outside this
/// range and are therefore classified as *moving*, another accepted V1 gap:
/// katamari has no SHA-256-repo test coverage to validate a wider bound
/// against), every character ASCII hex. A jj change id is 32 characters
/// drawn from a reverse-hex-style alphabet of 16 *letters* (see
/// `vcs::jj`'s tests for real examples, e.g.
/// `qrlotxzlnnttqlwvzsuyroxsmqlnvror`) that excludes every hex digit
/// `0`-`9` and `a`-`f` — every ordinary jj change id therefore contains at
/// least one character outside `[0-9a-f]` and fails this check, which is
/// exactly what makes `@`/`@-` (and any other change-id revset) correctly
/// classify as moving despite being a fixed-length identifier the same way
/// a commit hash superficially is.
fn looks_like_object_id(rev: &str) -> bool {
    (4..=40).contains(&rev.len()) && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbolic_revisions_are_moving() {
        for rev in ["HEAD", "HEAD~2", "main", "@", "@-"] {
            assert!(is_moving_revision(rev), "{rev} should be moving");
        }
    }

    /// A jj change id (letters-only alphabet, never all-hex) is moving —
    /// its *content* is mutable under amend even though the id itself is
    /// jj's stable identity for the change. See real examples in
    /// `vcs::jj`'s own tests (`parses_log_lines_and_drops_the_sentinel_root_change`).
    #[test]
    fn jj_change_ids_are_moving() {
        assert!(is_moving_revision("qrlotxzlnnttqlwvzsuyroxsmqlnvror"));
        assert!(is_moving_revision("ruwywzxwqswtomxrprtyprrosylzslyo"));
    }

    #[test]
    fn full_and_abbreviated_commit_hashes_are_not_moving() {
        assert!(!is_moving_revision(
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        ));
        assert!(!is_moving_revision("4b825dc"));
    }

    /// Ranges are excluded via the `..` check regardless of what their
    /// endpoints look like — the V1 boundary [`is_moving_revision`]'s docs
    /// describe, pinned down with both range syntaxes and an endpoint that
    /// would itself classify as moving in isolation.
    #[test]
    fn ranges_are_never_moving_even_with_moving_endpoints() {
        assert!(!is_moving_revision("HEAD~3..HEAD"));
        assert!(!is_moving_revision("main...feature"));
        assert!(!is_moving_revision("main..HEAD"));
    }

    /// [`looks_like_object_id`]'s length bound is inclusive on both ends —
    /// pinned at the boundary rather than only mid-range, so a future
    /// off-by-one regresses a test immediately.
    #[test]
    fn object_id_length_boundaries() {
        assert!(!is_moving_revision("abcd")); // 4 hex chars: shortest abbreviation
        assert!(is_moving_revision("abc")); // 3: too short to be an id, so moving
        let forty_hex = "a".repeat(40);
        assert!(!is_moving_revision(&forty_hex)); // 40: a full SHA-1
        let forty_one_hex = "a".repeat(41);
        assert!(is_moving_revision(&forty_one_hex)); // 41: too long, so moving
    }

    /// The documented false-negative: a branch/bookmark name that happens
    /// to be all lowercase hex digits within the id-length range is
    /// misclassified as immutable. Pinned down as a *known* gap, not an
    /// oversight — see [`is_moving_revision`]'s docs.
    #[test]
    fn hex_looking_branch_names_are_a_documented_false_negative() {
        assert!(!is_moving_revision("dead"));
        assert!(!is_moving_revision("face"));
    }
}
