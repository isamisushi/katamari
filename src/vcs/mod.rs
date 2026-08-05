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
    /// (HEAD, or the empty tree when the repo has no commits yet).
    fn working_tree_diff(&self) -> Result<String>;

    /// Absolute path to the repository root, used for display in the status
    /// bar.
    fn repo_root(&self) -> Result<PathBuf>;
}
