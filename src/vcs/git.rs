use super::{DiffSource, LogEntry, RevisionEntry};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Splits `git ls-files -z`'s NUL-delimited stdout into `PathBuf`s without
/// ever requiring the bytes to be valid UTF-8 — a legal filename on
/// Linux/macOS can contain arbitrary non-UTF-8 bytes, and [`String::from_utf8`]
/// over the *entire* buffer (the old approach) meant a single such filename
/// anywhere in the repo made [`GitSource::tracked_files`]/
/// [`GitSource::untracked_files`] fail outright — which
/// [`crate::doctor::scan_repo_files`] then propagated as one report-wide
/// error, aborting every per-language live-probe check regardless of how
/// many other, perfectly healthy languages were also present. Decoding
/// path-by-path instead means one bad name can never take down the rest of
/// the scan: [`crate::lsp::adapter::LangKey::detect`] only ever needs
/// [`Path::extension`], which stays available even when the rest of the
/// path isn't valid Unicode.
fn paths_from_nul_delimited(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|&b| b == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(path_from_bytes)
        .collect()
}

/// On Unix, [`std::ffi::OsStr`] is just an unvalidated byte string (no
/// encoding requirement at all — the kernel treats a path as bytes, full
/// stop), so this is lossless: the exact bytes git printed become the exact
/// bytes in the resulting `PathBuf`, valid UTF-8 or not.
#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(bytes))
}

/// Off Unix, [`std::ffi::OsStr`] *does* have an encoding (WTF-8, layered on
/// UTF-16 paths), so an arbitrary byte slice can't be wrapped losslessly the
/// way [`OsStrExt::from_bytes`] does on Unix. A lossy fallback is still
/// strictly better than the old behavior: it can turn a handful of bytes
/// into the Unicode replacement character instead of aborting the entire
/// scan over one filename.
#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

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
/// cheap to construct and safe to recreate per command — [`Clone`] for the
/// same reason [`super::jj::JjRepo`] is: [`super::LogBackend`] and any
/// [`crate::ui::log_view::LogView`] it hands to need their own owned copy.
#[derive(Clone)]
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

    /// Builds a [`GitSource`] directly from an already-known repository
    /// root, bypassing [`Self::discover`]'s own `git rev-parse` call — for
    /// callers (like [`super::LogBackend::detect`]) that already resolved
    /// the root some other way and would otherwise pay for the same lookup
    /// twice. `repo_root` is trusted as-is; an invalid one surfaces the same
    /// way it always would, the first time a command actually runs against
    /// it.
    pub fn at(repo_root: PathBuf) -> Self {
        Self { repo_root }
    }

    /// The repository root this instance was resolved against — infallible,
    /// unlike [`DiffSource::repo_root`] (which exists to satisfy that
    /// trait's signature and just clones this).
    pub fn repo_root_path(&self) -> &Path {
        &self.repo_root
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
    ///
    /// `pub(crate)` (not just this file's own callers) so [`crate::doctor`]'s
    /// extension scan can union this with [`Self::tracked_files`] — katamari
    /// reviews untracked files too (that's the whole point of
    /// [`Self::untracked_diff`]), so a language server health check that
    /// only looked at tracked files would miss "new file, LSP silent," the
    /// issue that motivated the doctor command in the first place.
    pub(crate) fn untracked_files(&self) -> Result<Vec<PathBuf>> {
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
        Ok(paths_from_nul_delimited(&output.stdout))
    }

    /// Every path git considers tracked (`git ls-files --cached`), relative
    /// to the repo root — the tracked half of [`crate::doctor`]'s extension
    /// scan; [`Self::untracked_files`] is the other half. Same `-z`
    /// NUL-delimited reasoning as that method.
    pub(crate) fn tracked_files(&self) -> Result<Vec<PathBuf>> {
        let output = self
            .git_command()
            .args(["ls-files", "--cached", "-z"])
            .output()
            .context("failed to run `git ls-files --cached`")?;

        if !output.status.success() {
            bail!(
                "git ls-files --cached failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(paths_from_nul_delimited(&output.stdout))
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

    /// How many files the working tree touches versus the review baseline —
    /// tracked changes plus untracked files, the same two-part definition
    /// [`Self::working_tree_diff`] uses, but computed via `git diff
    /// --name-only` (a path listing, not full patch text) instead of that
    /// method's per-file diffing — cheap enough to run just to decide
    /// whether [`Self::log`]'s synthetic "local changes" row exists at all,
    /// and to label it with a count, without paying for the patches
    /// themselves until a reviewer actually opens it.
    fn working_tree_change_count(&self) -> Result<usize> {
        let baseline = self.baseline()?;
        let output = self
            .git_command()
            .args(["diff", "--name-only", &baseline])
            .output()
            .context("failed to run `git diff --name-only`")?;
        if !output.status.success() {
            bail!(
                "git diff --name-only failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let text = String::from_utf8(output.stdout)
            .context("git diff --name-only produced non-UTF-8 output")?;
        let tracked = text.lines().filter(|l| !l.is_empty()).count();
        Ok(tracked + self.untracked_files()?.len())
    }

    /// Field separator for [`Self::log`]'s `git log --format` template —
    /// ASCII Unit Separator, the same choice [`super::jj::JjRepo`]'s op-log
    /// template makes and for the same reason: it can't appear in a commit
    /// subject typed as ordinary text.
    const LOG_FIELD_SEP: &'static str = "\u{1f}";

    /// `ktmr log`'s git-backed history: the synthetic
    /// [`LogEntry::LocalChanges`] row first (only when the working tree is
    /// actually dirty — see [`Self::working_tree_change_count`]), then up to
    /// `limit` commits reachable from `HEAD`, newest first. A repository
    /// with no commits yet reports an empty commit list rather than letting
    /// `git log` fail outright (it errors on a branch with no history) —
    /// still combined with a local-changes row if there's untracked/staged
    /// content to show even before the first commit.
    pub fn log(&self, limit: usize) -> Result<Vec<LogEntry>> {
        let mut entries = Vec::new();
        let changed = self.working_tree_change_count()?;
        if changed > 0 {
            entries.push(LogEntry::LocalChanges {
                changed_files: changed,
            });
        }

        if !self.rev_exists("HEAD")? {
            return Ok(entries);
        }

        let template = format!(
            "%H{sep}%h{sep}%an{sep}%at{sep}%D{sep}%s",
            sep = Self::LOG_FIELD_SEP
        );
        let output = self
            .git_command()
            .args(["log", "--no-color", "-n", &limit.to_string()])
            .arg(format!("--format={template}"))
            .output()
            .context("failed to run `git log`")?;
        if !output.status.success() {
            bail!(
                "git log failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let text = String::from_utf8(output.stdout).context("git log produced non-UTF-8 output")?;
        entries.extend(parse_git_log(&text).into_iter().map(LogEntry::Revision));
        Ok(entries)
    }
}

/// Parses [`GitSource::log`]'s raw, `\x1f`-field-separated `git log --format`
/// output into [`RevisionEntry`]s — a pure function so the format string and
/// its parsing stay verifiably in sync without a real git process (see this
/// module's tests).
fn parse_git_log(text: &str) -> Vec<RevisionEntry> {
    text.lines().filter_map(parse_git_log_line).collect()
}

fn parse_git_log_line(line: &str) -> Option<RevisionEntry> {
    let mut fields = line.splitn(6, GitSource::LOG_FIELD_SEP);
    let id = fields.next()?.to_owned();
    let short_id = fields.next()?.to_owned();
    let author = fields.next()?.to_owned();
    let time_unix = fields.next()?.parse().ok()?;
    let refs = parse_git_decoration(fields.next().unwrap_or_default());
    let summary = fields.next().unwrap_or_default().to_owned();
    Some(RevisionEntry {
        id,
        short_id,
        summary,
        author,
        time_unix,
        refs,
        // A git commit is never itself the working copy — see
        // `RevisionEntry::is_working_copy`'s docs.
        is_working_copy: false,
    })
}

/// Splits `git log --format=%D`'s decoration string (e.g. `"HEAD -> main,
/// tag: v1, origin/main"`) into individual ref names, in the order git
/// printed them. Kept exactly as git names them (`"HEAD -> main"` rather
/// than just `"main"`, `"tag: v1"` rather than just `"v1"`) — display-only
/// data, not something any call site parses back apart, so there's nothing
/// to gain by stripping git's own labels.
fn parse_git_decoration(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(", ").map(|s| s.to_owned()).collect()
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
    fn tracked_files_and_untracked_files_partition_the_working_tree() {
        // `crate::doctor`'s extension scan unions these two — pinning that
        // they're actually disjoint (a staged-but-uncommitted file is
        // "tracked" the moment it's added, not still "untracked") is what
        // keeps that union from double-counting a file.
        let dir = init_repo();
        std::fs::write(dir.path().join("committed.txt"), "a\n").unwrap();
        git(dir.path(), &["add", "committed.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);

        std::fs::write(dir.path().join("staged_new.txt"), "b\n").unwrap();
        git(dir.path(), &["add", "staged_new.txt"]);
        std::fs::write(dir.path().join("untracked.txt"), "c\n").unwrap();

        let source = GitSource::discover(dir.path()).unwrap();
        let tracked = source.tracked_files().unwrap();
        let untracked = source.untracked_files().unwrap();

        assert!(
            tracked.contains(&PathBuf::from("committed.txt")),
            "{tracked:?}"
        );
        assert!(
            tracked.contains(&PathBuf::from("staged_new.txt")),
            "{tracked:?}"
        );
        assert!(
            !tracked.contains(&PathBuf::from("untracked.txt")),
            "{tracked:?}"
        );

        assert_eq!(untracked, vec![PathBuf::from("untracked.txt")]);
    }

    /// A filename byte sequence that's legal on a Unix filesystem (no `/`
    /// or NUL) but not valid UTF-8 — `0xff` can never start a valid UTF-8
    /// sequence, so `String::from_utf8` over anything containing it always
    /// fails.
    #[cfg(unix)]
    fn non_utf8_name() -> &'static std::ffi::OsStr {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(b"\xffbad.txt")
    }

    #[cfg(unix)]
    #[test]
    fn untracked_files_decodes_a_non_utf8_filename_instead_of_erroring() {
        use std::os::unix::ffi::OsStrExt;
        let dir = init_repo();
        git(dir.path(), &["commit", "-q", "-m", "init", "--allow-empty"]);
        std::fs::write(dir.path().join(non_utf8_name()), "hello\n").unwrap();

        let source = GitSource::discover(dir.path()).unwrap();
        let files = source.untracked_files().unwrap();

        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].as_os_str().as_bytes(), non_utf8_name().as_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn tracked_files_decodes_a_non_utf8_filename_instead_of_erroring() {
        use std::os::unix::ffi::OsStrExt;
        let dir = init_repo();
        std::fs::write(dir.path().join(non_utf8_name()), "hello\n").unwrap();
        let status = Command::new("git")
            .current_dir(dir.path())
            .arg("add")
            .arg("--")
            .arg(non_utf8_name())
            .status()
            .expect("git must be on PATH for these tests");
        assert!(status.success());
        git(dir.path(), &["commit", "-q", "-m", "add non-utf8 file"]);

        let source = GitSource::discover(dir.path()).unwrap();
        let files = source.tracked_files().unwrap();

        assert!(
            files
                .iter()
                .any(|f| f.as_os_str().as_bytes() == non_utf8_name().as_bytes()),
            "{files:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_filename_does_not_abort_the_rest_of_the_untracked_scan() {
        // Regression for the doctor live-probe bug this was pulled out to
        // fix: one bad filename must not make `untracked_files` return
        // `Err`, taking every other, perfectly fine file down with it.
        let dir = init_repo();
        git(dir.path(), &["commit", "-q", "-m", "init", "--allow-empty"]);
        std::fs::write(dir.path().join("good.txt"), "a\n").unwrap();
        std::fs::write(dir.path().join(non_utf8_name()), "b\n").unwrap();

        let source = GitSource::discover(dir.path()).unwrap();
        let files = source.untracked_files().unwrap();

        assert!(files.contains(&PathBuf::from("good.txt")), "{files:?}");
        assert_eq!(files.len(), 2, "{files:?}");
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

    // ---- parse_git_log / parse_git_decoration --------------------------

    #[test]
    fn parses_a_log_line_with_no_decoration() {
        let sep = GitSource::LOG_FIELD_SEP;
        let line = format!("aaaa111{sep}aaaa{sep}Test{sep}1780000000{sep}{sep}first commit");
        let entries = parse_git_log(&line);
        assert_eq!(
            entries,
            vec![RevisionEntry {
                id: "aaaa111".to_owned(),
                short_id: "aaaa".to_owned(),
                summary: "first commit".to_owned(),
                author: "Test".to_owned(),
                time_unix: 1780000000,
                refs: Vec::new(),
                is_working_copy: false,
            }]
        );
    }

    #[test]
    fn parses_a_log_line_with_bookmarks_and_a_tag() {
        let sep = GitSource::LOG_FIELD_SEP;
        let line = format!(
            "bbbb222{sep}bbbb{sep}Test{sep}1780000001{sep}HEAD -> main, tag: v1{sep}second commit"
        );
        let entries = parse_git_log(&line);
        assert_eq!(
            entries[0].refs,
            vec!["HEAD -> main".to_owned(), "tag: v1".to_owned()]
        );
    }

    #[test]
    fn a_line_missing_fields_is_skipped_rather_than_panicking() {
        assert_eq!(parse_git_log("onlyonefield"), Vec::new());
    }

    #[test]
    fn empty_log_text_parses_to_no_entries() {
        assert_eq!(parse_git_log(""), Vec::new());
    }

    // ---- GitSource::log ---------------------------------------------------

    #[test]
    fn log_has_no_local_changes_row_when_the_working_tree_is_clean() {
        let dir = init_repo();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "first"]);

        let source = GitSource::discover(dir.path()).unwrap();
        let entries = source.log(10).unwrap();

        assert_eq!(entries.len(), 1, "entries: {entries:?}");
        assert!(matches!(&entries[0], LogEntry::Revision(r) if r.summary == "first"));
    }

    #[test]
    fn log_prepends_a_local_changes_row_when_the_working_tree_is_dirty() {
        let dir = init_repo();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "first"]);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("untracked.txt"), "new\n").unwrap();

        let source = GitSource::discover(dir.path()).unwrap();
        let entries = source.log(10).unwrap();

        assert_eq!(entries.len(), 2, "entries: {entries:?}");
        assert_eq!(entries[0], LogEntry::LocalChanges { changed_files: 2 });
        assert!(matches!(&entries[1], LogEntry::Revision(r) if r.summary == "first"));
    }

    #[test]
    fn log_on_a_repo_with_no_commits_yet_reports_no_revisions() {
        let dir = init_repo();
        let source = GitSource::discover(dir.path()).unwrap();
        assert_eq!(source.log(10).unwrap(), Vec::new());
    }

    #[test]
    fn log_on_a_repo_with_no_commits_yet_still_reports_local_changes() {
        let dir = init_repo();
        std::fs::write(dir.path().join("untracked.txt"), "new\n").unwrap();

        let source = GitSource::discover(dir.path()).unwrap();
        let entries = source.log(10).unwrap();

        assert_eq!(entries, vec![LogEntry::LocalChanges { changed_files: 1 }]);
    }
}
