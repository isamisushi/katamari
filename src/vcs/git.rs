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

/// Repo-relative path prefixes an AI coding agent commonly checks out a
/// disposable worktree under, *inside* the very repository being reviewed —
/// excluded from [`GitSource::untracked_files`] (and therefore
/// [`GitSource::working_tree_diff`]) by default, and from the file
/// watcher's registration/event filtering ([`crate::watch::spawn`]'s
/// `agent_workspace_prefixes` parameter threads this same list through).
/// Only ever prunes *untracked* content: a file under one of these paths
/// that's actually committed still reviews normally (see this module's
/// `a_committed_file_under_an_agent_workspace_prefix_still_reviews` test) —
/// the point is hiding a throwaway worktree's regenerated junk
/// (`node_modules`, build output, a second `target/`), not the worktree
/// itself if a reviewer chose to commit something there.
///
/// A pruned/abandoned worktree already loses its own `.git` (or never had
/// one un-gitignored), so [`crate::watch`]'s existing nested-checkout rule
/// stops protecting it the moment that happens — and unlike a `.gitignore`
/// entry, a reviewer can't be relied on to have added one for a directory
/// some *other* tool created without asking. This list is the non-negatable
/// (no `.gitignore` `!`-pattern can re-admit it — see
/// [`crate::watch::HARDCODED_EXCLUDES`] for the same stance on `.git`/
/// `target`/`node_modules`) backstop for exactly that gap.
///
/// Per-entry provenance (verified against each tool's own docs while this
/// feature was designed, not guessed):
/// - `.claude/worktrees/` — Claude Code's current, official default (its
///   docs tell users to gitignore this path, which is exactly the case this
///   list exists for: the moment a worktree gets pruned *without* that
///   gitignore entry, or the directory has already been un-gitignored,
///   nothing else protects it).
/// - `.claude/worktree/` (singular) — kept alongside the plural form even
///   though it conflicts with Claude Code's current docs: this shipped on a
///   bug report describing the singular form in the wild (an older version,
///   or a wrapper tool), and an unused prefix costs nothing.
/// - `.codex/worktree(s)/` — speculative insurance, not an observed Codex
///   CLI convention: Codex's real worktree root
///   (`$CODEX_HOME/worktrees`) lives *outside* any repo entirely, so these
///   two entries are here only in case some wrapper or future version ever
///   checks one out in-repo instead. Deliberately not `.cursor/worktrees/`
///   — Cursor's docs confirm a `.cursor/worktrees.json` *config* file at the
///   repo root but never state where the checkout directories themselves
///   land, so there's nothing confirmed to pin; a Cursor user who confirms
///   one can add it via `[diff] agent_workspace_extra` instead.
pub(crate) const DEFAULT_AGENT_WORKSPACE_PREFIXES: &[&str] = &[
    ".claude/worktrees",
    ".claude/worktree",
    ".codex/worktrees",
    ".codex/worktree",
];

/// Turns `[diff] agent_workspaces`/`agent_workspace_extra` (see
/// [`crate::config::DiffConfig`]) into the concrete prefix list
/// [`GitSource::with_agent_workspace_prefixes`]/`watch::spawn` actually
/// filter against — plain primitives rather than `&DiffConfig` itself, so
/// neither this module nor `watch` has to depend on `crate::config`'s
/// types. `enabled = false` collapses to an empty list rather than a
/// separate on/off flag threaded everywhere downstream: an empty prefix
/// list is already exactly "nothing is excluded" to every consumer
/// ([`matches_agent_workspace_prefix`] over an empty slice is always
/// `false`), so this is the one place that distinction gets resolved.
pub fn resolve_agent_workspace_prefixes(enabled: bool, extra: &[String]) -> Vec<PathBuf> {
    if !enabled {
        return Vec::new();
    }
    DEFAULT_AGENT_WORKSPACE_PREFIXES
        .iter()
        .map(PathBuf::from)
        .chain(extra.iter().map(PathBuf::from))
        .collect()
}

/// Whether `relative` (already repo-root-relative, as every path this is
/// called against already is — [`GitSource::untracked_files`]'s own output,
/// or `watch`'s `path.strip_prefix(repo_root)`) falls under one of
/// `prefixes`. Component-wise via [`Path::starts_with`], deliberately never
/// a raw string prefix check: `prefixes` is a list of *path* prefixes
/// (`.claude/worktrees` is two components), and a naive `str::starts_with`
/// would false-positive a sibling directory that merely shares the same
/// leading characters (`.claude/worktrees2/` must never match
/// `.claude/worktrees`) — see this module's own boundary test.
pub fn matches_agent_workspace_prefix(relative: &Path, prefixes: &[PathBuf]) -> bool {
    prefixes.iter().any(|prefix| relative.starts_with(prefix))
}

/// [`GitSource::untracked_exclusion_summary`]'s result: how many untracked
/// paths `agent_workspace_prefixes` hid from [`GitSource::untracked_files`],
/// and which of the configured prefixes actually matched at least one of
/// them (in list order, each named at most once) — enough for `ui::run`'s
/// one-time startup note to name both the count and *where* the hidden
/// content lives, not just that something was hidden.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UntrackedExclusionSummary {
    pub count: usize,
    pub prefixes: Vec<PathBuf>,
}

/// A [`DiffSource`] backed by an installed `git` binary invoked as a
/// subprocess. Holds nothing but the resolved repository root, so it stays
/// cheap to construct and safe to recreate per command — [`Clone`] for the
/// same reason [`super::jj::JjRepo`] is: [`super::LogBackend`] and any
/// [`crate::ui::log_view::LogView`] it hands to need their own owned copy.
#[derive(Clone)]
pub struct GitSource {
    repo_root: PathBuf,
    /// Repo-relative path prefixes [`Self::untracked_files`] hides —
    /// defaulted to [`DEFAULT_AGENT_WORKSPACE_PREFIXES`] by both
    /// [`Self::discover`] and [`Self::at`] so every one of this type's many
    /// construction sites gets "filter agent workspaces by default" for
    /// free, with zero signature changes at any of them. A caller that must
    /// honor a *non-default* `[diff] agent_workspaces`/`agent_workspace_extra`
    /// config (disabled, or an extended list) needs
    /// [`Self::with_agent_workspace_prefixes`] instead — see that method's
    /// docs; a new call site that constructs a fresh `GitSource` and calls
    /// `working_tree_diff`/`log` without going through it will silently
    /// disagree with a user's config the moment it's non-default.
    agent_workspace_prefixes: Vec<PathBuf>,
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
            agent_workspace_prefixes: resolve_agent_workspace_prefixes(true, &[]),
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
        Self {
            repo_root,
            agent_workspace_prefixes: resolve_agent_workspace_prefixes(true, &[]),
        }
    }

    /// Overrides the built-in default [`Self::discover`]/[`Self::at`] seed
    /// [`Self::agent_workspace_prefixes`] with — the door a session that
    /// must honor a non-default `[diff] agent_workspaces`/
    /// `agent_workspace_extra` config uses, via
    /// [`crate::vcs::git::resolve_agent_workspace_prefixes`]. Takes `self`
    /// by value and returns it (builder-style) rather than `&mut self`
    /// purely so every call site can stay a one-line expression
    /// (`GitSource::at(root).with_agent_workspace_prefixes(prefixes)`)
    /// alongside the existing `at`/`discover` constructors, matching how
    /// those already read.
    pub fn with_agent_workspace_prefixes(mut self, prefixes: Vec<PathBuf>) -> Self {
        self.agent_workspace_prefixes = prefixes;
        self
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

    /// Issue #8: the full object id `rev` currently resolves to, or
    /// `Ok(None)` if it doesn't resolve to anything — [`Self::rev_exists`]'s
    /// sibling, except this one hands back the id itself rather than
    /// discarding it, since detecting *which* commit a moving revision like
    /// `HEAD` points at right now (not just whether it points at something)
    /// is the whole point of `ui::mod`'s moving-scope refresh. `Ok(None)`
    /// rather than an `Err` for "doesn't resolve" mirrors `rev_exists`'s own
    /// choice: a revision that no longer resolves (a branch got deleted, a
    /// detached `HEAD` rewound past what it named) is exactly as unsurprising
    /// here as "doesn't exist yet" is there, not a failure worth a status-bar
    /// error of its own.
    pub fn resolve(&self, rev: &str) -> Result<Option<String>> {
        let output = self
            .git_command()
            .args(["rev-parse", "--verify", "-q"])
            .arg(rev)
            .output()
            .context("failed to run `git rev-parse`")?;
        if !output.status.success() {
            return Ok(None);
        }
        let id = String::from_utf8(output.stdout)
            .context("git rev-parse produced non-UTF-8 output")?
            .trim()
            .to_owned();
        Ok(Some(id))
    }

    /// `vcs::base::detect_base`'s second-priority fallback (after a
    /// colocated jj repo's own `trunk()`): `git symbolic-ref -q --short
    /// refs/remotes/origin/HEAD`, which prints e.g. `"origin/main"` for a
    /// normal clone. Purely local — it reads a ref file under `.git`, no
    /// network touched — and `Ok(None)` on failure (a fresh `git init`, or a
    /// clone whose remote never got an advertised default branch set, e.g.
    /// via `git remote set-head origin -a`) rather than an `Err`, the same
    /// "doesn't resolve is unsurprising, not a failure" stance
    /// [`Self::resolve`] already takes.
    pub fn origin_head_branch(&self) -> Result<Option<String>> {
        let output = self
            .git_command()
            .args(["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"])
            .output()
            .context("failed to run `git symbolic-ref`")?;
        if !output.status.success() {
            return Ok(None);
        }
        let name = String::from_utf8(output.stdout)
            .context("git symbolic-ref produced non-UTF-8 output")?
            .trim()
            .to_owned();
        Ok((!name.is_empty()).then_some(name))
    }

    /// `git rev-list --count <base>..<head>` — deliberately the *two*-dot,
    /// ahead-only form, not three-dot: three-dot's count is the *symmetric*
    /// ahead-plus-behind total (empirically verified while researching
    /// `vcs::base`: `2` on a fixture where two-dot correctly gave `1`),
    /// which is the wrong number for "+N commits" on top of `base`. Feeds
    /// [`crate::vcs::base::DetectedBase::ahead`], both for the scope-menu/
    /// empty-state gate (`ahead == 0` — no entry, no hint) and the status-
    /// bar label's `(+N)`.
    pub fn ahead_count(&self, base: &str, head: &str) -> Result<usize> {
        let output = self
            .git_command()
            .args(["rev-list", "--count", &format!("{base}..{head}")])
            .output()
            .context("failed to run `git rev-list`")?;
        if !output.status.success() {
            bail!(
                "git rev-list --count {base}..{head} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        String::from_utf8(output.stdout)
            .context("git rev-list produced non-UTF-8 output")?
            .trim()
            .parse::<usize>()
            .context("git rev-list --count produced a non-numeric result")
    }

    /// The checked-out branch's name for display — `git branch
    /// --show-current`, which prints an *empty string with a successful
    /// exit code* on a detached `HEAD` (verified empirically; unlike
    /// `symbolic-ref`, it never fails), turned into the literal `"HEAD"`
    /// here rather than surfaced as a blank status-bar label. Display only:
    /// `vcs::base`'s diff-arg/ahead-count formulas always compare against
    /// the symbolic ref `"HEAD"` directly, never whatever branch name
    /// happens to be checked out, so a detached `HEAD` never affects
    /// correctness here, only what the branch-vs-base label reads.
    pub fn current_branch_display(&self) -> Result<String> {
        let output = self
            .git_command()
            .args(["branch", "--show-current"])
            .output()
            .context("failed to run `git branch --show-current`")?;
        if !output.status.success() {
            bail!(
                "git branch --show-current failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let name = String::from_utf8(output.stdout)
            .context("git branch --show-current produced non-UTF-8 output")?
            .trim()
            .to_owned();
        Ok(if name.is_empty() {
            "HEAD".to_owned()
        } else {
            name
        })
    }

    /// Issue #8: resolves `relative` (`HEAD`, `refs`, `packed-refs`, ...)
    /// against this repository's *actual* git directory via `git rev-parse
    /// --git-path <relative>`, never by hand-composing `<something>/.git/
    /// <relative>` ourselves (see [`crate::watch::spawn_revision_watcher`],
    /// the one caller): a git *worktree*'s `.git` is a file, not a
    /// directory, and per-worktree state (`HEAD`, `logs/HEAD`) lives under a
    /// private gitdir elsewhere entirely (typically `<main-repo>/.git/
    /// worktrees/<name>/HEAD`) while shared state (`refs/`, `packed-refs`)
    /// still lives in the common dir — only git itself reliably knows which
    /// is which. Confirmed empirically against this project's own worktree
    /// setup while building this feature.
    ///
    /// `git rev-parse --git-path` itself prints a path relative to the `-C`
    /// working directory it was invoked with (this repository's root) for a
    /// plain, non-worktree repo, but an *absolute* one the moment the real
    /// git-dir lives somewhere else entirely (the worktree case above) — so
    /// the result is joined onto [`Self::repo_root`] here purely to
    /// guarantee an absolute, directly `Path::exists`/`notify`-usable
    /// return value in the common case; [`PathBuf::join`] treats an already-
    /// absolute second path as a full replacement rather than concatenating
    /// it (see its own docs), so this join is a no-op precisely when git
    /// already did the worktree-aware resolution itself, never a second,
    /// competing guess at where the real path lives.
    pub fn git_path(&self, relative: &str) -> Result<PathBuf> {
        let output = self
            .git_command()
            .args(["rev-parse", "--git-path"])
            .arg(relative)
            .output()
            .context("failed to run `git rev-parse --git-path`")?;
        if !output.status.success() {
            bail!(
                "git rev-parse --git-path {relative} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let path = String::from_utf8(output.stdout)
            .context("git rev-parse --git-path produced non-UTF-8 output")?
            .trim()
            .to_owned();
        Ok(self.repo_root.join(path))
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

    /// The raw `git ls-files --others --exclude-standard -z` listing,
    /// before [`Self::agent_workspace_prefixes`] filtering — split out so
    /// [`Self::untracked_files`] and [`Self::untracked_exclusion_summary`]
    /// (which needs to see what got filtered *out*, not just what's left)
    /// share one subprocess-invocation-and-parse rather than drifting apart.
    fn all_untracked_paths(&self) -> Result<Vec<PathBuf>> {
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

    /// Paths under `--exclude-standard` (i.e. not gitignored) that exist on
    /// disk but aren't tracked, relative to the repo root — minus anything
    /// under [`Self::agent_workspace_prefixes`] (default: an agent CLI's
    /// disposable in-repo worktree; see [`DEFAULT_AGENT_WORKSPACE_PREFIXES`]).
    /// Untracked-only: a *committed* file living under one of those prefixes
    /// is never touched here at all (this method has nothing to do with
    /// tracked content — see [`Self::tracked_files`]), so a reviewer who
    /// deliberately committed something inside an agent workspace still sees
    /// it exactly like any other tracked change.
    ///
    /// `pub(crate)` (not just this file's own callers) so [`crate::doctor`]'s
    /// extension scan can union this with [`Self::tracked_files`] — katamari
    /// reviews untracked files too (that's the whole point of
    /// [`Self::untracked_diff`]), so a language server health check that
    /// only looked at tracked files would miss "new file, LSP silent," the
    /// issue that motivated the doctor command in the first place.
    pub(crate) fn untracked_files(&self) -> Result<Vec<PathBuf>> {
        Ok(self
            .all_untracked_paths()?
            .into_iter()
            .filter(|p| !matches_agent_workspace_prefix(p, &self.agent_workspace_prefixes))
            .collect())
    }

    /// How much [`Self::untracked_files`]' agent-workspace filtering
    /// actually hid, right now — a second `git ls-files` listing (paying for
    /// one extra subprocess call, the same "pay a little extra for a count,
    /// separately from the full diff text" trade-off
    /// [`Self::working_tree_change_count`] already makes for [`Self::log`]'s
    /// badge) rather than reshaping [`Self::untracked_files`]'s return type,
    /// which several callers ([`Self::working_tree_diff`],
    /// [`Self::working_tree_change_count`], [`crate::doctor`]'s extension
    /// scan) all unpack as a bare `Vec<PathBuf>` today. Used once, at TUI
    /// startup, for the "N files hidden under <prefix>" status note — see
    /// `ui::mod::run`'s startup_status chain — never on every refresh, so
    /// the extra subprocess call is a one-time session-start cost, not a
    /// per-keystroke one.
    pub fn untracked_exclusion_summary(&self) -> Result<UntrackedExclusionSummary> {
        if self.agent_workspace_prefixes.is_empty() {
            return Ok(UntrackedExclusionSummary::default());
        }
        let mut count = 0;
        let mut prefixes: Vec<PathBuf> = Vec::new();
        for path in self.all_untracked_paths()? {
            let Some(prefix) = self
                .agent_workspace_prefixes
                .iter()
                .find(|prefix| path.starts_with(prefix))
            else {
                continue;
            };
            count += 1;
            if !prefixes.contains(prefix) {
                prefixes.push(prefix.clone());
            }
        }
        Ok(UntrackedExclusionSummary { count, prefixes })
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

    // ---- origin_head_branch / ahead_count / current_branch_display -----

    /// Bare "upstream" + a push from a working clone — the minimal shape
    /// `git clone` needs to populate `refs/remotes/origin/HEAD` on its own,
    /// the same sequence `vcs::base`'s module docs and
    /// `tests/support/fixture.rs::branch_ahead_of_main_repo` both build for
    /// real E2E coverage. The bare repo's `HEAD` must be pointed at `main`
    /// *before* the first push (a freshly `--bare`-init'd repo defaults to
    /// `refs/heads/master`) — `git init -b main --bare` does that directly,
    /// sidestepping the `symbolic-ref` dance a plain `--bare` init would
    /// otherwise need.
    fn init_upstream_and_clone() -> (TempDir, TempDir) {
        let upstream = tempfile::tempdir().expect("create temp dir");
        git(upstream.path(), &["init", "-q", "-b", "main", "--bare"]);

        // Not `init_repo()`: that helper's plain `git init -q` takes
        // whatever `init.defaultBranch` the host's git resolves to (often
        // `master`, never guaranteed `main`) — this fixture needs the seed
        // checked out on a branch literally named `main` so the push below
        // lands on the same ref name the bare repo's `HEAD` already points
        // at.
        let seed = tempfile::tempdir().expect("create temp dir");
        git(seed.path(), &["init", "-q", "-b", "main"]);
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test"]);
        std::fs::write(seed.path().join("a.txt"), "one\n").unwrap();
        git(seed.path(), &["add", "a.txt"]);
        git(seed.path(), &["commit", "-q", "-m", "first"]);
        git(
            seed.path(),
            &["remote", "add", "origin", upstream.path().to_str().unwrap()],
        );
        git(seed.path(), &["push", "-q", "origin", "main"]);

        let clone_dir = tempfile::tempdir().expect("create temp dir");
        let status = Command::new("git")
            .args([
                "clone",
                "-q",
                upstream.path().to_str().unwrap(),
                clone_dir.path().to_str().unwrap(),
            ])
            .status()
            .expect("git must be on PATH for these tests");
        assert!(status.success(), "git clone failed");
        (upstream, clone_dir)
    }

    #[test]
    fn origin_head_branch_reports_the_clones_default_branch() {
        let (_upstream, clone_dir) = init_upstream_and_clone();
        let source = GitSource::discover(clone_dir.path()).unwrap();
        assert_eq!(
            source.origin_head_branch().unwrap().as_deref(),
            Some("origin/main")
        );
    }

    #[test]
    fn origin_head_branch_is_none_without_a_remote() {
        let dir = init_repo();
        git(dir.path(), &["commit", "-q", "-m", "init", "--allow-empty"]);
        let source = GitSource::discover(dir.path()).unwrap();
        assert_eq!(source.origin_head_branch().unwrap(), None);
    }

    #[test]
    fn ahead_count_is_two_dot_not_three_dot() {
        let (_upstream, clone_dir) = init_upstream_and_clone();
        let root = clone_dir.path();
        git(root, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("b.txt"), "two\n").unwrap();
        git(root, &["add", "b.txt"]);
        git(root, &["commit", "-q", "-m", "second"]);

        let source = GitSource::discover(root).unwrap();
        assert_eq!(source.ahead_count("origin/main", "HEAD").unwrap(), 1);
    }

    #[test]
    fn ahead_count_is_zero_on_the_base_itself() {
        let (_upstream, clone_dir) = init_upstream_and_clone();
        let source = GitSource::discover(clone_dir.path()).unwrap();
        assert_eq!(source.ahead_count("origin/main", "HEAD").unwrap(), 0);
    }

    #[test]
    fn current_branch_display_reports_the_checked_out_branch() {
        let dir = init_repo();
        git(dir.path(), &["commit", "-q", "-m", "init", "--allow-empty"]);
        git(dir.path(), &["checkout", "-q", "-b", "feature"]);
        let source = GitSource::discover(dir.path()).unwrap();
        assert_eq!(source.current_branch_display().unwrap(), "feature");
    }

    #[test]
    fn current_branch_display_falls_back_to_head_when_detached() {
        let dir = init_repo();
        git(dir.path(), &["commit", "-q", "-m", "init", "--allow-empty"]);
        git(dir.path(), &["checkout", "-q", "--detach", "HEAD"]);
        let source = GitSource::discover(dir.path()).unwrap();
        assert_eq!(source.current_branch_display().unwrap(), "HEAD");
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

    // ---- GitSource::resolve / git_path (issue #8) --------------------------

    #[test]
    fn resolve_matches_rev_parse() {
        let dir = init_repo();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "first"]);

        let source = GitSource::discover(dir.path()).unwrap();
        let resolved = source.resolve("HEAD").unwrap();

        let output = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let expected = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        assert_eq!(resolved, Some(expected));
    }

    #[test]
    fn resolve_of_a_nonexistent_revision_is_ok_none() {
        let dir = init_repo();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "first"]);

        let source = GitSource::discover(dir.path()).unwrap();
        assert_eq!(source.resolve("nonexistent-branch").unwrap(), None);
    }

    #[test]
    fn resolve_differs_after_an_amend() {
        let dir = init_repo();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "first"]);

        let source = GitSource::discover(dir.path()).unwrap();
        let before = source.resolve("HEAD").unwrap();

        git(
            dir.path(),
            &["commit", "-q", "--amend", "-m", "first (amended)"],
        );
        let after = source.resolve("HEAD").unwrap();

        assert_ne!(before, after, "an amend must change what HEAD resolves to");
    }

    #[test]
    fn git_path_resolves_relative_to_the_actual_git_dir() {
        let dir = init_repo();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "first"]);

        let source = GitSource::discover(dir.path()).unwrap();
        let head_path = source.git_path("HEAD").unwrap();

        assert!(
            head_path.is_file(),
            "git-path HEAD must resolve to a real file: {head_path:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&head_path).unwrap(),
            std::fs::read_to_string(dir.path().join(".git").join("HEAD")).unwrap(),
            "a plain (non-worktree) repo's git-path HEAD is just .git/HEAD"
        );
    }

    // ---- matches_agent_workspace_prefix / resolve_agent_workspace_prefixes --

    #[test]
    fn matches_agent_workspace_prefix_matches_a_direct_and_a_nested_descendant() {
        let prefixes = vec![PathBuf::from(".claude/worktrees")];
        assert!(matches_agent_workspace_prefix(
            &PathBuf::from(".claude/worktrees/agentA/README.md"),
            &prefixes
        ));
        // Nested arbitrarily deep — the whole point is hiding *everything*
        // a worktree regenerates underneath it, not just its top level.
        assert!(matches_agent_workspace_prefix(
            &PathBuf::from(".claude/worktrees/agentA/node_modules/pkg/index.js"),
            &prefixes
        ));
    }

    /// The boundary case a raw string-prefix check would get wrong: a
    /// sibling directory that merely shares the same leading characters
    /// must never match. `Path::starts_with` is component-wise, so
    /// `.claude/worktreesX` shares no component with `.claude/worktrees`
    /// past `.claude` and correctly fails to match.
    #[test]
    fn matches_agent_workspace_prefix_does_not_match_a_sibling_with_a_shared_string_prefix() {
        let prefixes = vec![PathBuf::from(".claude/worktrees")];
        assert!(!matches_agent_workspace_prefix(
            &PathBuf::from(".claude/worktreesX/file.txt"),
            &prefixes
        ));
        assert!(!matches_agent_workspace_prefix(
            &PathBuf::from(".claude/worktree/file.txt"),
            &[PathBuf::from(".claude/worktrees")],
        ));
    }

    /// A trailing slash on the configured prefix string (an easy typo in
    /// `agent_workspace_extra`, e.g. `"vendor/agent/"`) must match exactly
    /// like the same prefix without one — `PathBuf`'s own component
    /// splitting never produces an empty trailing component from it.
    #[test]
    fn matches_agent_workspace_prefix_ignores_a_trailing_slash_on_the_prefix() {
        let prefixes = vec![PathBuf::from("vendor/agent/")];
        assert!(matches_agent_workspace_prefix(
            &PathBuf::from("vendor/agent/scratch.txt"),
            &prefixes
        ));
    }

    #[test]
    fn resolve_agent_workspace_prefixes_disabled_is_always_empty() {
        assert!(resolve_agent_workspace_prefixes(false, &[]).is_empty());
        assert!(
            resolve_agent_workspace_prefixes(false, &["extra/one".to_owned()]).is_empty(),
            "disabled must win over an extra list too, not just the built-in default"
        );
    }

    #[test]
    fn resolve_agent_workspace_prefixes_enabled_appends_extra_after_the_built_ins() {
        let resolved = resolve_agent_workspace_prefixes(true, &["vendor/agent".to_owned()]);
        let built_ins: Vec<PathBuf> = DEFAULT_AGENT_WORKSPACE_PREFIXES
            .iter()
            .map(PathBuf::from)
            .collect();
        assert!(
            resolved.starts_with(&built_ins),
            "the built-in list must stay intact, not be replaced: {resolved:?}"
        );
        assert!(resolved.contains(&PathBuf::from("vendor/agent")));
    }

    // ---- GitSource agent-workspace filtering ----------------------------

    #[test]
    fn untracked_files_excludes_agent_workspace_content_by_default() {
        let dir = init_repo();
        git(dir.path(), &["commit", "-q", "-m", "init", "--allow-empty"]);
        std::fs::create_dir_all(dir.path().join(".claude/worktrees/agentA")).unwrap();
        std::fs::write(
            dir.path().join(".claude/worktrees/agentA/junk.txt"),
            "regenerated build junk\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("ordinary.txt"), "a real new file\n").unwrap();

        let source = GitSource::discover(dir.path()).unwrap();
        let files = source.untracked_files().unwrap();

        assert!(
            files.contains(&PathBuf::from("ordinary.txt")),
            "an ordinary untracked file must still be listed: {files:?}"
        );
        assert!(
            !files.iter().any(|f| f.starts_with(".claude/worktrees")),
            "agent-workspace content must be filtered out by default: {files:?}"
        );
    }

    /// Hard requirement: this filtering is untracked-only. A file that's
    /// actually committed under an agent-workspace prefix (a reviewer chose
    /// to commit something there, deliberately or not) is tracked content —
    /// `working_tree_diff` never routes tracked changes through
    /// `untracked_files` at all (see that method's own docs), so its
    /// modification must show up in the diff exactly like any other tracked
    /// edit.
    #[test]
    fn a_committed_file_under_an_agent_workspace_prefix_still_reviews() {
        let dir = init_repo();
        std::fs::create_dir_all(dir.path().join(".claude/worktrees/agentA")).unwrap();
        std::fs::write(
            dir.path().join(".claude/worktrees/agentA/README.md"),
            "one\n",
        )
        .unwrap();
        git(dir.path(), &["add", "-A"]);
        git(
            dir.path(),
            &["commit", "-q", "-m", "commit a worktree file on purpose"],
        );

        std::fs::write(
            dir.path().join(".claude/worktrees/agentA/README.md"),
            "one\ntwo\n",
        )
        .unwrap();

        let source = GitSource::discover(dir.path()).unwrap();
        let diff = source.working_tree_diff().unwrap();

        assert!(
            diff.contains("+two"),
            "a tracked edit under an agent-workspace prefix must still appear: {diff}"
        );
    }

    #[test]
    fn with_agent_workspace_prefixes_empty_disables_the_filtering() {
        let dir = init_repo();
        git(dir.path(), &["commit", "-q", "-m", "init", "--allow-empty"]);
        std::fs::create_dir_all(dir.path().join(".claude/worktrees/agentA")).unwrap();
        std::fs::write(
            dir.path().join(".claude/worktrees/agentA/junk.txt"),
            "junk\n",
        )
        .unwrap();

        let source = GitSource::discover(dir.path())
            .unwrap()
            .with_agent_workspace_prefixes(Vec::new());
        let files = source.untracked_files().unwrap();

        assert!(
            files.contains(&PathBuf::from(".claude/worktrees/agentA/junk.txt")),
            "an empty prefix list (agent_workspaces = false) must disable filtering: {files:?}"
        );
    }

    #[test]
    fn untracked_exclusion_summary_reports_count_and_matched_prefixes() {
        let dir = init_repo();
        git(dir.path(), &["commit", "-q", "-m", "init", "--allow-empty"]);
        std::fs::create_dir_all(dir.path().join(".claude/worktrees/agentA")).unwrap();
        std::fs::create_dir_all(dir.path().join(".claude/worktree/agentB")).unwrap();
        std::fs::write(dir.path().join(".claude/worktrees/agentA/a.txt"), "junk\n").unwrap();
        std::fs::write(dir.path().join(".claude/worktree/agentB/b.txt"), "junk\n").unwrap();
        std::fs::write(dir.path().join("ordinary.txt"), "kept\n").unwrap();

        let source = GitSource::discover(dir.path()).unwrap();
        let summary = source.untracked_exclusion_summary().unwrap();

        assert_eq!(summary.count, 2, "{summary:?}");
        assert!(
            summary
                .prefixes
                .contains(&PathBuf::from(".claude/worktrees"))
        );
        assert!(
            summary
                .prefixes
                .contains(&PathBuf::from(".claude/worktree"))
        );
    }

    #[test]
    fn untracked_exclusion_summary_is_zero_when_nothing_is_excluded() {
        let dir = init_repo();
        git(dir.path(), &["commit", "-q", "-m", "init", "--allow-empty"]);
        std::fs::write(dir.path().join("ordinary.txt"), "kept\n").unwrap();

        let source = GitSource::discover(dir.path()).unwrap();
        let summary = source.untracked_exclusion_summary().unwrap();

        assert_eq!(summary, UntrackedExclusionSummary::default());
    }

    #[test]
    fn untracked_exclusion_summary_is_zero_when_filtering_is_disabled() {
        let dir = init_repo();
        git(dir.path(), &["commit", "-q", "-m", "init", "--allow-empty"]);
        std::fs::create_dir_all(dir.path().join(".claude/worktrees/agentA")).unwrap();
        std::fs::write(
            dir.path().join(".claude/worktrees/agentA/junk.txt"),
            "junk\n",
        )
        .unwrap();

        let source = GitSource::discover(dir.path())
            .unwrap()
            .with_agent_workspace_prefixes(Vec::new());
        let summary = source.untracked_exclusion_summary().unwrap();

        assert_eq!(summary, UntrackedExclusionSummary::default());
    }
}
