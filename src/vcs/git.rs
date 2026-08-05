use super::DiffSource;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Git's well-known empty-tree object hash. It is deterministic (it's just
/// the SHA-1 of an empty tree object, the same in every git repository ever
/// created — `git hash-object -t tree /dev/null` always reproduces it), so
/// hardcoding it avoids a subprocess call. Diffing against it is how we
/// answer "what's in the working tree" for a repo that has no HEAD yet: git
/// diff always needs a tree-ish on one side, and there is no commit to use.
const EMPTY_TREE_OID: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A [`DiffSource`] backed by an installed `git` binary invoked as a
/// subprocess. Holds nothing but the resolved repository root, so it stays
/// cheap to construct and safe to recreate per command.
pub struct GitSource {
    repo_root: PathBuf,
}

impl GitSource {
    /// Resolves the repository containing `start_dir`. Fails with a clear
    /// message if `start_dir` isn't inside a git repository or `git` isn't
    /// on PATH.
    pub fn discover(start_dir: &Path) -> Result<Self> {
        let output = Command::new("git")
            .arg("-C")
            .arg(start_dir)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("failed to run `git`; is it installed and on PATH?")?;

        if !output.status.success() {
            bail!(
                "not a git repository: {}\n{}",
                start_dir.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let root = String::from_utf8(output.stdout)
            .context("git printed a non-UTF-8 repository path")?
            .trim()
            .to_owned();
        Ok(Self {
            repo_root: PathBuf::from(root),
        })
    }

    fn has_head_commit(&self) -> Result<bool> {
        // `.output()` captures stdout/stderr instead of inheriting the
        // parent's, so the commit hash `rev-parse` prints on success never
        // leaks onto our own stdout.
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["rev-parse", "--verify", "-q", "HEAD"])
            .output()
            .context("failed to run `git rev-parse`")?;
        Ok(output.status.success())
    }

    fn diff_against(&self, tree_ish: &str) -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["diff", "--no-color", "--no-ext-diff", tree_ish])
            .output()
            .context("failed to run `git diff`")?;

        if !output.status.success() {
            bail!(
                "git diff failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        String::from_utf8(output.stdout).context("git diff produced non-UTF-8 output")
    }
}

impl DiffSource for GitSource {
    fn working_tree_diff(&self) -> Result<String> {
        let baseline = if self.has_head_commit()? {
            "HEAD"
        } else {
            EMPTY_TREE_OID
        };
        self.diff_against(baseline)
    }

    fn repo_root(&self) -> Result<PathBuf> {
        Ok(self.repo_root.clone())
    }
}
