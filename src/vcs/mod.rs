//! Abstracts over version-control backends so `diff::model` and the UI never
//! call `git` directly. M1 ships one implementation ([`git::GitSource`]);
//! later milestones can add a `jj` backend behind the same trait without
//! touching anything upstream of it.

pub mod git;

use anyhow::Result;
use std::path::PathBuf;

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
