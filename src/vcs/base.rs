//! `ktmr diff --branch`'s no-network base detection: which ref a branch is
//! reviewed *against* — `[diff] base` in config, then a colocated jj repo's
//! own `trunk()`, then `git`'s locally-recorded `refs/remotes/origin/HEAD`,
//! then a local `main`, then a local `master`, then a clear error naming
//! both remedies. Every step here is a cheap, local git/jj call (a
//! `rev-parse`/`symbolic-ref`/`jj log` against already-fetched refs) —
//! nothing in this module ever touches the network, unlike `vcs::github`'s
//! `gh pr diff`.
//!
//! Split into an I/O-doing outer function ([`detect_base`]) and a pure
//! precedence core ([`pick_base`]), the same `update.rs` split pattern
//! [`crate::update::upgrade_command`]/[`crate::update::detect_upgrade_command`]
//! use — every ordering case (config wins, jj beats `origin/HEAD`,
//! `origin/HEAD` beats `main`, `main` beats `master`, nothing resolves) is a
//! plain unit test with no real git/jj process involved.

use super::git::GitSource;
use super::jj::JjRepo;

/// The base a branch is being reviewed against, plus how far `HEAD` has
/// moved past it — the one value every `--branch` call site (the CLI flag,
/// the scope-menu entry, the `Action::ReviewBranchVsBase` keybinding, and
/// the empty-state placeholder's live hint) is built from, so all four stay
/// byte-for-byte in agreement about what "the base" means for this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedBase {
    /// Display *and* diff-arg text: a git ref (`"main"`, `"origin/main"`),
    /// a jj bookmark name (`"main"` — see [`trunk_bookmark_name`] for why
    /// this is never the raw `"main main@origin"` jj itself can report), or
    /// `[diff] base`'s configured string verbatim.
    pub name: String,
    /// Which backend resolved `name` — a jj revset (fed straight into
    /// `fork_point({name} | @)`) versus a plain git ref (fed into
    /// `{name}...HEAD`). Mirrors [`crate::ui::app::RevisionScope::via_jj`]'s
    /// own reasoning for storing this explicitly rather than re-deriving it
    /// from "is a jj repo detected": a colocated repo's `--branch` still
    /// means git when `configured` was validated against git (see
    /// [`detect_base`]'s docs on how `configured` picks a backend).
    pub via_jj: bool,
    /// Commits on `HEAD`'s side that aren't on `name`'s — `git rev-list
    /// --count <base>..HEAD` (git) or a jj revset's match count (jj), both
    /// deliberately the *two*-sided/ahead-only form (see
    /// [`crate::vcs::git::GitSource::ahead_count`]'s docs for why the
    /// three-dot/symmetric form is the wrong number here). `0` collapses
    /// "HEAD equals base" and "HEAD is strictly behind base" into the same
    /// gate every caller already needs (menu-greying, the empty-state
    /// hint): both cases have literally nothing of the branch's own to
    /// review, so no separate equality check is needed anywhere.
    pub ahead: usize,
}

/// Detects the base for `--branch`/the scope-menu entry/the empty-state
/// hint, per this module's docs' precedence order. `configured` is
/// `config.diff.base` — when `Some`, it is *authoritative*: no fallthrough
/// to jj/git detection on a resolve failure, just a hard error naming the
/// bad value (a typo'd `[diff] base` should never silently fall back to
/// guessing).
///
/// `configured` is validated against *whichever backend this call already
/// has*: a jj revset when `jj_repo` is `Some`, else a plain git ref — the
/// same backend-follows-repo-shape split `-r`/the plain positional `range`
/// already draw in `main.rs`. A colocated jj repo with a `[diff] base =
/// "develop"` meant as a *git* branch name is a real, if unlikely, surprise
/// this shares with that existing precedent rather than introduces new.
pub fn detect_base(
    git: &GitSource,
    jj_repo: Option<&JjRepo>,
    configured: Option<&str>,
) -> Result<DetectedBase, String> {
    if let Some(configured) = configured {
        return detect_configured(git, jj_repo, configured);
    }

    let jj_trunk = jj_repo.and_then(|repo| {
        let bookmarks = repo.trunk_bookmarks().ok()?;
        trunk_bookmark_name(&bookmarks)
    });
    let origin_head = git.origin_head_branch().map_err(|e| e.to_string())?;
    // `origin_head_branch` only reads the *symref file* under
    // `refs/remotes/origin/HEAD` — it never checks that the ref it points
    // at still exists. A stale/pruned remote-tracking branch (the local
    // repo's `origin/HEAD` symref was never refreshed with `git remote
    // set-head origin -a` after the upstream default branch moved or was
    // deleted) would otherwise be handed to `pick_base` as a seemingly-good
    // candidate, only to hard-fail downstream in `ahead_count`'s `git
    // rev-list` with raw git stderr — instead of falling through to the
    // already-verified-present `main`/`master` below, same as an unset
    // `origin/HEAD` already does.
    let origin_head = origin_head.filter(|name| git.resolve(name).ok().flatten().is_some());
    let main_exists = git.resolve("main").map_err(|e| e.to_string())?.is_some();
    let master_exists = git.resolve("master").map_err(|e| e.to_string())?.is_some();

    let (via_jj, name) = pick_base(
        jj_trunk.as_deref(),
        origin_head.as_deref(),
        main_exists,
        master_exists,
    )
    .ok_or_else(|| {
        "no base detected — set [diff] base in config, or run \
         `git remote set-head origin -a`"
            .to_owned()
    })?;
    let name = name.to_owned();

    let ahead = if via_jj {
        // `pick_base` only ever returns `via_jj: true` when `jj_trunk` —
        // itself sourced from `jj_repo` — was `Some`, so `jj_repo` is
        // guaranteed `Some` here too.
        ahead_count_jj(jj_repo.expect("via_jj implies jj_repo is Some"), &name)
    } else {
        git.ahead_count(&name, "HEAD").map_err(|e| e.to_string())
    }?;

    Ok(DetectedBase {
        name,
        via_jj,
        ahead,
    })
}

fn detect_configured(
    git: &GitSource,
    jj_repo: Option<&JjRepo>,
    configured: &str,
) -> Result<DetectedBase, String> {
    let via_jj = jj_repo.is_some();
    let resolves = if via_jj {
        jj_repo
            .expect("via_jj implies jj_repo is Some")
            .resolve_commit_id(configured)
            .map_err(|e| e.to_string())?
            .is_some()
    } else {
        git.resolve(configured)
            .map_err(|e| e.to_string())?
            .is_some()
    };
    if !resolves {
        return Err(format!(
            "[diff] base = \"{configured}\" does not resolve to a ref in this repository"
        ));
    }

    let ahead = if via_jj {
        ahead_count_jj(jj_repo.expect("checked above"), configured)
    } else {
        git.ahead_count(configured, "HEAD")
            .map_err(|e| e.to_string())
    }?;

    Ok(DetectedBase {
        name: configured.to_owned(),
        via_jj,
        ahead,
    })
}

/// `<base>..@`'s match count via [`JjRepo::resolve_commit_id`] — no new jj
/// method needed: that method already runs `jj log -r <revset> ...` and
/// returns the newline-joined ids of every match (or `None` for zero), so
/// counting lines is the ahead count. Mirrors
/// [`crate::vcs::git::GitSource::ahead_count`]'s two-dot/ahead-only shape,
/// just expressed in jj's own revset syntax rather than a `rev-list` flag.
fn ahead_count_jj(jj_repo: &JjRepo, base: &str) -> Result<usize, String> {
    Ok(jj_repo
        .resolve_commit_id(&format!("{base}..@"))
        .map_err(|e| e.to_string())?
        .map(|ids| ids.lines().count())
        .unwrap_or(0))
}

/// The no-`[diff] base` fallback chain, as a pure function of
/// already-detected candidates — no process spawns of its own, so every
/// ordering case is a plain unit test. Returns `(via_jj, name)`; `None`
/// when nothing in the chain resolved at all.
fn pick_base<'a>(
    jj_trunk: Option<&'a str>,
    origin_head: Option<&'a str>,
    main_exists: bool,
    master_exists: bool,
) -> Option<(bool, &'a str)> {
    if let Some(name) = jj_trunk {
        return Some((true, name));
    }
    if let Some(name) = origin_head {
        return Some((false, name));
    }
    if main_exists {
        return Some((false, "main"));
    }
    if master_exists {
        return Some((false, "master"));
    }
    None
}

/// Picks a single clean revset/display name out of [`JjRepo::trunk_bookmarks`]'s
/// raw, potentially multi-name field — see that method's docs for why more
/// than one name (`"main main@origin"`) is the *common* case, not an edge
/// one. Prefers a bare local name (nicer to read, and jj accepts either
/// form equally as a revset) over a `name@remote` one when both are
/// present, and strips a `@remote` suffix off whichever it does pick, so
/// the result is always a single plain name usable both as a revset
/// (`fork_point(main | @)`) and as display text. `None` for the empty,
/// degenerate case (`trunk()` fell back to `root()` — no default-remote
/// bookmark ever resolved).
fn trunk_bookmark_name(bookmarks: &str) -> Option<String> {
    let bookmarks = bookmarks.trim();
    if bookmarks.is_empty() {
        return None;
    }
    let picked = bookmarks
        .split_whitespace()
        .find(|s| !s.contains('@'))
        .or_else(|| bookmarks.split_whitespace().next())?;
    Some(picked.split('@').next().unwrap_or(picked).to_owned())
}

/// `"<head-display> vs <base> (+N)"` — the status-bar label every
/// `--branch` entry point builds from the same [`DetectedBase`], so the
/// CLI flag, the scope-menu entry, and the keybinding can never drift in
/// wording. `head_display` is resolved by the caller (git:
/// [`crate::vcs::git::GitSource::current_branch_display`]; jj: the literal
/// `"@"`, this app's existing convention for the working-copy commit — see
/// `crate::ui::scope_menu`'s revision-input placeholder text) since which
/// backend resolved `HEAD`'s own display name isn't this function's
/// concern.
pub fn branch_vs_base_label(head_display: &str, base: &DetectedBase) -> String {
    format!("{head_display} vs {} (+{})", base.name, base.ahead)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- pick_base: precedence core, no real git/jj process -------------

    #[test]
    fn jj_trunk_wins_outright() {
        assert_eq!(
            pick_base(Some("main"), Some("origin/main"), true, true),
            Some((true, "main"))
        );
    }

    #[test]
    fn origin_head_beats_local_main_and_master() {
        assert_eq!(
            pick_base(None, Some("origin/main"), true, true),
            Some((false, "origin/main"))
        );
    }

    #[test]
    fn local_main_beats_master_once_jj_and_origin_head_are_absent() {
        assert_eq!(pick_base(None, None, true, true), Some((false, "main")));
    }

    #[test]
    fn master_is_the_last_resort() {
        assert_eq!(pick_base(None, None, false, true), Some((false, "master")));
    }

    #[test]
    fn nothing_resolves_is_none() {
        assert_eq!(pick_base(None, None, false, false), None);
    }

    // ---- trunk_bookmark_name ---------------------------------------------

    #[test]
    fn empty_bookmarks_is_the_degenerate_case() {
        assert_eq!(trunk_bookmark_name(""), None);
        assert_eq!(trunk_bookmark_name("   "), None);
    }

    #[test]
    fn prefers_a_bare_local_name_over_a_remote_tracking_one() {
        assert_eq!(
            trunk_bookmark_name("main main@origin").as_deref(),
            Some("main")
        );
        // Order-independent: whichever token is the bare local one wins,
        // not just "whichever comes first".
        assert_eq!(
            trunk_bookmark_name("main@origin main").as_deref(),
            Some("main")
        );
    }

    #[test]
    fn strips_the_remote_suffix_when_only_a_remote_tracking_bookmark_exists() {
        assert_eq!(trunk_bookmark_name("main@origin").as_deref(), Some("main"));
    }

    // ---- branch_vs_base_label ---------------------------------------------

    #[test]
    fn label_formats_head_vs_base_with_the_ahead_count() {
        let base = DetectedBase {
            name: "main".to_owned(),
            via_jj: false,
            ahead: 3,
        };
        assert_eq!(
            branch_vs_base_label("feature", &base),
            "feature vs main (+3)"
        );
    }

    #[test]
    fn label_uses_the_at_sign_convention_for_jj_head_display() {
        let base = DetectedBase {
            name: "main".to_owned(),
            via_jj: true,
            ahead: 1,
        };
        assert_eq!(branch_vs_base_label("@", &base), "@ vs main (+1)");
    }

    // ---- detect_base: `configured` is authoritative, no fallthrough -----
    //
    // Real-tempdir integration tests (like `vcs::git::git`'s own), not
    // `pick_base` unit tests: `configured`'s validate-or-hard-error branch
    // is a separate code path in `detect_base` that never touches
    // `pick_base` at all (see that function's own docs), so it needs its
    // own coverage against a real `GitSource`.

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("git must be on PATH for these tests");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    fn init_repo_with_a_branch() -> (tempfile::TempDir, GitSource) {
        let dir = tempfile::tempdir().expect("create temp dir");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "first"]);
        git(dir.path(), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        git(dir.path(), &["commit", "-q", "-am", "second"]);
        let source = GitSource::at(dir.path().to_owned());
        (dir, source)
    }

    #[test]
    fn configured_base_wins_outright_and_reports_ahead() {
        let (_dir, git) = init_repo_with_a_branch();
        let base = detect_base(&git, None, Some("main")).unwrap();
        assert_eq!(base.name, "main");
        assert!(!base.via_jj);
        assert_eq!(base.ahead, 1);
    }

    #[test]
    fn configured_base_that_does_not_resolve_is_a_hard_error_with_no_fallthrough() {
        let (_dir, git) = init_repo_with_a_branch();
        // `master` doesn't exist in this fixture (the branch is `main`) —
        // proving this errors, rather than silently falling back to
        // `origin_head`/`main`/`master` detection the way an *unset*
        // `[diff] base` would, is the whole point of this test.
        let err = detect_base(&git, None, Some("nonexistent-branch")).unwrap_err();
        assert!(err.contains("nonexistent-branch"), "{err}");
    }

    #[test]
    fn no_config_and_no_jj_falls_back_to_local_main() {
        let (_dir, git) = init_repo_with_a_branch();
        let base = detect_base(&git, None, None).unwrap();
        assert_eq!(base.name, "main");
        assert!(!base.via_jj);
        assert_eq!(base.ahead, 1);
    }

    #[test]
    fn stale_origin_head_falls_through_to_local_main_instead_of_hard_failing() {
        // A dangling `refs/remotes/origin/HEAD` symref — the ref file
        // itself exists (so `origin_head_branch` happily reports
        // `"origin/main"`) but its target, `refs/remotes/origin/main`, was
        // pruned/deleted server-side and the local symref was never
        // refreshed via `git remote set-head origin -a`. This is the exact
        // shape a routine "default branch renamed" or "stale local clone"
        // situation leaves behind, with no real remote needed to reproduce
        // it: `symbolic-ref` only ever writes the ref file, it never
        // requires the target to resolve.
        let (dir, git) = init_repo_with_a_branch();
        self::git(
            dir.path(),
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        // `refs/remotes/origin/main` itself is never created, so the
        // symref's target is dangling — `origin_head_branch` still returns
        // `Some("origin/main")`, but it must not resolve to anything.
        assert_eq!(git.resolve("origin/main").unwrap(), None);

        let base = detect_base(&git, None, None).unwrap();
        assert_eq!(base.name, "main", "should fall through to local main");
        assert!(!base.via_jj);
        assert_eq!(base.ahead, 1);
    }
}
