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
/// The same trick handles a root commit's `range_diff`, which has no parent
/// commit to diff against.
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

    /// A `git` invocation rooted at this repository, with path quoting
    /// disabled: git's default (`core.quotepath=true`) octal-escapes any
    /// path byte outside 7-bit ASCII in its output (e.g. `日本語.txt`
    /// becomes `"\346\227\245\346\234\254\350\252\236.txt"` in a `diff
    /// --git` header), which would corrupt every non-ASCII filename the
    /// parser and UI see. Every git call that can produce a path in its
    /// output goes through this constructor so the override is never
    /// forgotten on a new call site.
    fn git_command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.repo_root)
            .args(["-c", "core.quotepath=false"]);
        cmd
    }

    /// Whether `rev` resolves to a real object. Used both to detect a
    /// repository with no commits yet (`HEAD` doesn't exist) and, in
    /// [`Self::range_diff`], to detect a root commit (`<rev>^` doesn't
    /// exist).
    fn rev_exists(&self, rev: &str) -> Result<bool> {
        // `.output()` captures stdout/stderr instead of inheriting the
        // parent's, so the commit hash `rev-parse` prints on success never
        // leaks onto our own stdout.
        let output = self
            .git_command()
            .args(["rev-parse", "--verify", "-q"])
            .arg(rev)
            .output()
            .context("failed to run `git rev-parse`")?;
        Ok(output.status.success())
    }

    /// `HEAD` for a repository with at least one commit, or the empty tree
    /// for one that doesn't — the tree-ish every other method here diffs
    /// against.
    fn baseline(&self) -> Result<String> {
        Ok(if self.rev_exists("HEAD")? {
            "HEAD".to_owned()
        } else {
            EMPTY_TREE_OID.to_owned()
        })
    }

    /// Runs `git diff --no-color --no-ext-diff <args>` and returns stdout.
    /// Plain `git diff` (unlike `--no-index`, see [`Self::untracked_diff`])
    /// always exits 0 whether or not it found differences, so any nonzero
    /// status here is a real failure.
    fn run_diff(&self, args: &[&str]) -> Result<String> {
        let output = self
            .git_command()
            .arg("diff")
            .arg("--no-color")
            .arg("--no-ext-diff")
            .args(args)
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

    /// Paths under `--exclude-standard` (i.e. not gitignored) that exist on
    /// disk but aren't tracked, relative to the repo root. `-z` NUL-delimits
    /// the output instead of the default newline-delimited, quoted-for-shell
    /// format, so filenames with unusual bytes (spaces, non-ASCII) come back
    /// exactly as they are on disk rather than octal-escaped.
    fn untracked_files(&self) -> Result<Vec<PathBuf>> {
        let output = self
            .git_command()
            .args(["ls-files", "--others", "--exclude-standard", "-z"])
            .output()
            .context("failed to run `git ls-files`")?;

        if !output.status.success() {
            bail!(
                "git ls-files failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let raw =
            String::from_utf8(output.stdout).context("git ls-files produced non-UTF-8 output")?;
        Ok(raw.split_terminator('\0').map(PathBuf::from).collect())
    }

    /// A single untracked file's content as an all-add pseudo-diff, by
    /// diffing it against `/dev/null` outside of git's index
    /// (`--no-index`). This is the same mechanism `git add -N` plus `git
    /// diff` would produce, without actually touching the index — and for
    /// binary files, it's git's own binary-detection logic, so the parser's
    /// existing `Binary files ... differ` handling covers untracked binaries
    /// for free.
    ///
    /// Unlike plain `git diff`, `--no-index` exits 1 when it finds
    /// differences (which it always will here, since one side is empty) and
    /// only exits nonzero-and-not-1 on a real failure (e.g. a path git
    /// can't read).
    fn untracked_diff(&self, path: &Path) -> Result<String> {
        let output = self
            .git_command()
            .args(["diff", "--no-color", "--no-ext-diff", "--no-index", "--"])
            .arg("/dev/null")
            .arg(path)
            .output()
            .context("failed to run `git diff --no-index`")?;

        match output.status.code() {
            Some(0) | Some(1) => {}
            _ => bail!(
                "git diff --no-index failed for {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
        String::from_utf8(output.stdout)
            .with_context(|| format!("git diff produced non-UTF-8 output for {}", path.display()))
    }
}

/// Where a [`GitSource::range_diff`] revspec sends the underlying `git
/// diff` call. Split out from `range_diff` as a pure function of the input
/// string so the `..`-vs-single-rev heuristic is unit-testable without a
/// real git process.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RangePlan {
    /// Two-dot or three-dot range syntax git already understands natively:
    /// pass it through as `git diff <range>` unchanged.
    PassThrough,
    /// A single revision: show that commit's own changes, i.e. `git diff
    /// <rev>^ <rev>`. `parent_arg` is `<rev>^`; [`GitSource::range_diff`]
    /// still has to check whether it actually resolves (a root commit has no
    /// parent) before using it.
    SingleRev { parent_arg: String },
}

fn plan_range(range: &str) -> RangePlan {
    if range.contains("..") {
        RangePlan::PassThrough
    } else {
        RangePlan::SingleRev {
            parent_arg: format!("{range}^"),
        }
    }
}

/// Appends a trailing newline if `s` is non-empty and doesn't already end
/// with one, so per-file diff chunks concatenate cleanly regardless of
/// whether the git subprocess that produced them included a final newline.
fn ensure_trailing_newline(mut s: String) -> String {
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

impl DiffSource for GitSource {
    fn working_tree_diff(&self) -> Result<String> {
        let baseline = self.baseline()?;
        let mut text = ensure_trailing_newline(self.run_diff(&[&baseline])?);
        for path in self.untracked_files()? {
            text.push_str(&ensure_trailing_newline(self.untracked_diff(&path)?));
        }
        Ok(text)
    }

    fn staged_diff(&self) -> Result<String> {
        let baseline = self.baseline()?;
        self.run_diff(&["--cached", &baseline])
    }

    fn range_diff(&self, range: &str) -> Result<String> {
        match plan_range(range) {
            RangePlan::PassThrough => self.run_diff(&[range]),
            RangePlan::SingleRev { parent_arg } => {
                let base = if self.rev_exists(&parent_arg)? {
                    parent_arg
                } else {
                    EMPTY_TREE_OID.to_owned()
                };
                self.run_diff(&[&base, range])
            }
        }
    }

    fn repo_root(&self) -> Result<PathBuf> {
        Ok(self.repo_root.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn range_with_two_dot_syntax_passes_through() {
        assert_eq!(plan_range("HEAD~3..HEAD"), RangePlan::PassThrough);
    }

    #[test]
    fn range_with_three_dot_syntax_passes_through() {
        assert_eq!(plan_range("main...feature"), RangePlan::PassThrough);
    }

    #[test]
    fn single_rev_asks_for_its_parent() {
        assert_eq!(
            plan_range("HEAD~1"),
            RangePlan::SingleRev {
                parent_arg: "HEAD~1^".to_owned()
            }
        );
    }

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .expect("git must be on PATH for these tests");
        assert!(status.success(), "git {args:?} failed in {repo:?}");
    }

    /// A throwaway repo with a fixed test identity, so commits succeed
    /// without depending on the host's global git config.
    fn init_repo() -> TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        dir
    }

    #[test]
    fn untracked_files_appear_as_new_file_diffs_including_utf8_names() {
        let dir = init_repo();
        std::fs::write(dir.path().join("tracked.txt"), "a\n").unwrap();
        git(dir.path(), &["add", "tracked.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);

        std::fs::write(dir.path().join("new.txt"), "hello\n").unwrap();
        std::fs::write(dir.path().join("日本語.txt"), "こんにちは\n").unwrap();

        let source = GitSource::discover(dir.path()).unwrap();
        let diff = source.working_tree_diff().unwrap();

        assert!(diff.contains("+hello"), "diff:\n{diff}");
        assert!(diff.contains("日本語.txt"), "diff:\n{diff}");
        assert!(diff.contains("こんにちは"), "diff:\n{diff}");
    }

    #[test]
    fn untracked_binary_file_gets_a_binary_marker_not_dumped_bytes() {
        let dir = init_repo();
        git(dir.path(), &["commit", "-q", "-m", "init", "--allow-empty"]);
        std::fs::write(dir.path().join("blob.bin"), [0u8, 159, 146, 150, 255]).unwrap();

        let source = GitSource::discover(dir.path()).unwrap();
        let diff = source.working_tree_diff().unwrap();

        assert!(diff.contains("Binary files"), "diff:\n{diff}");
        assert!(
            !diff.as_bytes().contains(&0u8),
            "raw bytes leaked into diff text"
        );
    }

    #[test]
    fn untracked_files_respect_gitignore() {
        let dir = init_repo();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        git(dir.path(), &["add", ".gitignore"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.path().join("ignored.txt"), "should not appear\n").unwrap();

        let source = GitSource::discover(dir.path()).unwrap();
        let diff = source.working_tree_diff().unwrap();

        assert!(!diff.contains("ignored.txt"), "diff:\n{diff}");
    }

    #[test]
    fn staged_diff_shows_index_changes_not_further_working_tree_edits() {
        let dir = init_repo();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);

        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        std::fs::write(dir.path().join("a.txt"), "three\n").unwrap(); // unstaged on top

        let source = GitSource::discover(dir.path()).unwrap();
        let staged = source.staged_diff().unwrap();

        assert!(staged.contains("+two"), "staged:\n{staged}");
        assert!(!staged.contains("+three"), "staged:\n{staged}");
    }

    #[test]
    fn range_diff_shows_a_single_commits_own_changes() {
        let dir = init_repo();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "first"]);
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "second"]);

        let source = GitSource::discover(dir.path()).unwrap();
        let diff = source.range_diff("HEAD").unwrap();

        assert!(diff.contains("-one"), "diff:\n{diff}");
        assert!(diff.contains("+two"), "diff:\n{diff}");
    }

    #[test]
    fn range_diff_on_root_commit_diffs_against_empty_tree() {
        let dir = init_repo();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "root"]);

        let source = GitSource::discover(dir.path()).unwrap();
        let diff = source.range_diff("HEAD").unwrap();

        assert!(diff.contains("+one"), "diff:\n{diff}");
    }

    #[test]
    fn range_diff_with_double_dot_passes_through_to_git() {
        let dir = init_repo();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "first"]);
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "second"]);

        let source = GitSource::discover(dir.path()).unwrap();
        let diff = source.range_diff("HEAD~1..HEAD").unwrap();

        assert!(diff.contains("+two"), "diff:\n{diff}");
    }
}
