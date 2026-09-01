//! Throwaway git repositories for the E2E suite to point `ktmr diff` at.
//! Every fixture uses plain `.txt`/`.md` content deliberately — never
//! `.rs`/`.ts`/`.py`/`.go` — so [`katamari`'s `Language::detect`] never
//! matches, which means [`LspManager::warm_up`] never has anything eligible
//! to open and no language server is ever resolved, installed, or spawned.
//! An E2E suite that accidentally depended on network access (an
//! auto-install) or a locally installed toolchain would be flaky in exactly
//! the ways this milestone is trying to eliminate.
//!
//! [`lsp_readiness_repo`]/[`lsp_readiness_repo_with_definition_target`] are
//! the one deliberate exception: issue #11's readiness coverage needs a
//! real, controllable server lifecycle to press hover/definition/references
//! against while it's still starting, which no built-in language could give
//! this suite without an auto-install, network access, or a locally
//! installed toolchain — exactly what every other fixture here exists to
//! avoid. They stay safe the same way a custom `[lsp.servers.<id>]` entry
//! always is: a fabricated file extension no built-in
//! [`katamari`'s `Language::detect`] has ever heard of, routed to a small
//! script this repository controls end to end (see
//! `support::fake_lsp_server`'s docs) rather than a real language server.

use std::path::Path;
use std::process::Command;

/// A repository directory that outlives the [`tempfile::TempDir`] guard
/// dropped alongside it — kept together so a fixture's lifetime is tied to
/// the value a test holds, the same RAII pattern [`tempfile::TempDir`]
/// itself uses.
pub struct FixtureRepo {
    dir: tempfile::TempDir,
}

impl FixtureRepo {
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// `git rev-parse --short <rev>`'s output — for a test that needs a
    /// deterministic revision to type into the scope-menu popup's
    /// "Revision…" input. Commit hashes aren't deterministic across runs
    /// (they hash the commit's author/committer timestamps among other
    /// things), so a test can never hardcode one; it has to ask the fixture
    /// for whatever hash it actually produced.
    pub fn commit_hash(&self, rev: &str) -> String {
        let output = Command::new("git")
            .args(["rev-parse", "--short", rev])
            .current_dir(self.path())
            .output()
            .expect("failed to spawn git — is it on PATH?");
        assert!(
            output.status.success(),
            "git rev-parse --short {rev} failed in {}:\nstderr: {}",
            self.path().display(),
            String::from_utf8_lossy(&output.stderr),
        );
        String::from_utf8(output.stdout)
            .expect("git rev-parse produced non-UTF-8 output")
            .trim()
            .to_owned()
    }
}

/// Runs `git <args>` in `dir` with a fixed, throwaway identity (`-c
/// user.email`/`-c user.name`, not a real `~/.gitconfig`, which the test
/// process's real environment may or may not have configured) and panics
/// with the captured output on failure — a fixture that failed to build is a
/// test-infrastructure bug, not something a test body should have to check
/// for.
///
/// `pub` (unlike every other helper in this file) so `tests/e2e/moving_scope.rs`
/// can drive a real `git commit --amend` *after* a harness has already
/// spawned against a fixture built here — issue #8's whole point is a
/// commit changing out from under an already-open session, which no
/// pre-baked fixture function can express on its own.
pub fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args([
            "-c",
            "user.email=e2e@katamari.test",
            "-c",
            "user.name=katamari e2e",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn git — is it on PATH?");
    assert!(
        output.status.success(),
        "git {args:?} failed in {}:\nstdout: {}\nstderr: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("katamari-e2e-repo-")
        .tempdir()
        .expect("failed to create fixture tempdir");
    git(dir.path(), &["init", "-q"]);
    dir
}

/// A small repo with an initial commit plus uncommitted working-tree edits
/// to plain-text files — enough content for `ktmr diff` to show a real diff
/// (added/changed/context lines, more than one file) without pulling in any
/// language-specific machinery.
pub fn basic_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    std::fs::write(
        root.join("README.md"),
        "# Sample project\n\nThis is line two.\nThis is line three.\nThis is line four.\n",
    )
    .unwrap();
    std::fs::write(root.join("notes.txt"), "alpha\nbeta\ngamma\ndelta\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    // Working-tree edits: one changed file, one new (untracked) file — the
    // shape a reviewer's session normally opens onto.
    std::fs::write(
        root.join("README.md"),
        "# Sample project\n\nThis is line two, updated.\nThis is line three.\nThis is line four.\nA brand new line five.\n",
    )
    .unwrap();
    std::fs::write(root.join("todo.txt"), "- write more fixtures\n").unwrap();

    FixtureRepo { dir }
}

/// A single commit ("first") and nothing else uncommitted — the issue #8
/// live-refresh E2E fixture (`tests/e2e/moving_scope.rs`). Deliberately
/// *without* `basic_repo`'s dirty working tree: that test amends `HEAD`
/// itself with `git commit --amend` after the harness is already spawned,
/// and a perfectly clean tree beforehand means `git add -A` can never sweep
/// in an edit this test didn't make, so each amend's content is exactly,
/// unambiguously what the test wrote.
pub fn moving_scope_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    std::fs::write(root.join("notes.txt"), "alpha\nbeta\ngamma\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "first"]);

    FixtureRepo { dir }
}

/// A single commit and nothing uncommitted — `ktmr diff`'s default
/// working-tree scope with literally nothing to review, for the
/// empty-diff-pane placeholder E2E coverage (`tests/e2e/rendering.rs`).
/// Builds the identical shape [`moving_scope_repo`] does for its own,
/// unrelated reason (amending `HEAD` after a harness is already spawned
/// against it) — kept as its own function rather than reused so a future
/// change to that test's needs can't silently change what this one
/// exercises too.
pub fn clean_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    std::fs::write(root.join("notes.txt"), "alpha\nbeta\ngamma\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "first"]);

    FixtureRepo { dir }
}

/// Shaped for `tests/e2e/watch_filtering.rs`: exercises the filtered
/// registration walk's two pruning rules — a gitignored directory and a
/// fake nested checkout — alongside one ordinary tracked file a test can
/// edit to prove live refresh still works at all, in the same session a
/// negative assertion about the other two relies on (see
/// `tests/e2e/moving_scope.rs`'s docs on why a positive proof and a
/// negative assertion belong in one sequenced session rather than two
/// independent tests).
///
/// Both `ignored/` and `agent-worktree/` are built as they'd appear
/// *before* a session ever spawns — the initial registration walk (not
/// dynamic re-registration) is what must prune them, since only that walk
/// runs deep enough to ever reach directories that already existed at
/// startup.
///
/// Both are also gitignored, `agent-worktree/` included — the realistic
/// shape a real `.claude/worktree/*` setup would already need regardless
/// of live refresh (an un-gitignored nested checkout would flood *every*
/// `git status`/`ktmr diff` with its entire untracked tree as "new files,"
/// watch feature or not), and the only shape an E2E assertion phrased as
/// "this content never renders" can prove *deterministically* end to end:
/// `handle_watch_refresh` re-derives the *whole* working-tree diff on
/// every trigger, from whatever reason, so an un-gitignored nested
/// checkout's current content would still surface the moment *any*
/// unrelated refresh happens to run — which says nothing about whether
/// the nested checkout's own change was what woke that refresh up
/// (`walk_admits`'s actual job), only about git's own untracked-file
/// listing. That distinction is what the unit tests
/// (`walk_admits_skips_a_nested_checkout_with_a_dot_git_file`/
/// `_directory`) exist to isolate, gitignore-free, at the function level;
/// this fixture's job is only the realistic end-to-end shape.
pub fn watch_filtering_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    std::fs::write(root.join("tracked.txt"), "alpha\nbeta\ngamma\n").unwrap();
    std::fs::write(root.join(".gitignore"), "ignored/\nagent-worktree/\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    // An uncommitted edit, already part of the diff at spawn time — the
    // same shape `basic_repo`'s `README.md` edit has: something the test
    // can `wait_for_text` on immediately (an unedited `tracked.txt` would
    // have no diff at all yet, and so wouldn't appear on screen to wait
    // for), and an already-visible line to overwrite with a marker later
    // rather than appending below into a collapsed unchanged-context gap.
    std::fs::write(root.join("tracked.txt"), "alpha, updated\nbeta\ngamma\n").unwrap();

    std::fs::create_dir_all(root.join("ignored")).unwrap();
    std::fs::write(root.join("ignored").join("output.txt"), "junk\n").unwrap();

    let worktree = root.join("agent-worktree");
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    std::fs::write(worktree.join(".git"), "gitdir: /nowhere/in/particular\n").unwrap();
    // `.txt`, not a real source extension — this module's own docs on why
    // every fixture avoids one (no `Language::detect` match means no LSP
    // warm-up/spawn) apply just as much to a file inside a directory this
    // test expects to never even be watched.
    std::fs::write(worktree.join("src").join("main.txt"), "placeholder\n").unwrap();

    FixtureRepo { dir }
}

/// A minimal repo with one un-gitignored nested checkout
/// (`worktree/.git`, a plain file — the shape a real linked worktree's
/// always is), for `ktmr watch-check`-driven coverage of
/// `watch::walk_admits`'s nested-checkout rule specifically, isolated from
/// gitignore-based exclusion the way [`watch_filtering_repo`]'s own
/// (gitignored) nested checkout can't be at the TUI level — see that
/// function's docs on why an un-gitignored nested checkout's content can
/// leak into a *full* working-tree diff regardless of which path
/// triggered it, and why `watch-check` (which never runs `git diff` at
/// all, only reports what `watch::spawn` itself detected) is the level
/// that can actually prove this rule deterministically end to end.
/// Nothing here needs to be tracked or committed at all — `watch-check`
/// has no diff concept, only raw filesystem events.
pub fn nested_checkout_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    let worktree = root.join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(worktree.join(".git"), "gitdir: /nowhere/in/particular\n").unwrap();
    std::fs::write(worktree.join("placeholder.txt"), "placeholder\n").unwrap();

    FixtureRepo { dir }
}

/// A repo whose `.gitignore` excludes `build/` wholesale but re-admits
/// `build/keep/` via negation (`build/\n!build/keep/\n`) — the common
/// "keep one grandfathered subdirectory inside an otherwise-ignored
/// directory" idiom (a config file inside `build/`/`dist/`/`vendor/`,
/// say), built with `build/keep/file.txt` already present *before* the
/// session ever spawns, the same "only the initial registration walk ever
/// reaches a pre-existing directory" reasoning [`watch_filtering_repo`]'s
/// own docs explain. For `ktmr watch-check`-driven coverage of the
/// registration walk's Finding-#2 regression: pruning descent into
/// `build/` the moment `build/` itself fails to admit its own watch would
/// make `build/keep/` — which *does* admit one, by the same `Gitignore`
/// `is_excluded` already resolves correctly — unreachable for the walk to
/// ever register at all.
pub fn negated_gitignore_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    std::fs::write(root.join(".gitignore"), "build/\n!build/keep/\n").unwrap();
    std::fs::create_dir_all(root.join("build").join("keep")).unwrap();
    std::fs::write(
        root.join("build").join("keep").join("file.txt"),
        "original\n",
    )
    .unwrap();

    FixtureRepo { dir }
}

/// Whether `jj` is on `PATH` — [`python3_available`]'s jj-side counterpart,
/// the same self-skip pattern for the same reason: `jj` is mise-managed
/// (`mise.toml`'s `[tools]` pins a version), so it resolves in this
/// project's own dev/CI environment, but a contributor's machine might not
/// have it at all, and this suite must stay green either way. Every
/// jj-backed E2E test (`tests/e2e/moving_scope.rs`'s jj case,
/// `tests/e2e/timeline.rs`) checks this first and skips — with an
/// `eprintln!`, so the skip is visible under `--nocapture` — rather than
/// fails, mirroring `src/vcs/jj.rs`'s and `src/ui/timeline_view.rs`'s own
/// real-jj unit tests.
pub fn jj_available() -> bool {
    std::process::Command::new("jj")
        .arg("--version")
        .output()
        .is_ok()
}

/// Runs `jj <args>` in `dir` and panics with the captured output on
/// failure — [`git`]'s jj-side counterpart, for the same "a fixture that
/// failed to build is a test-infrastructure bug, not something a test body
/// should have to check for" reason. Unlike [`git`], no `-c user.*`
/// override: jj's identity comes from repo-scoped config
/// ([`init_jj_repo`] sets it once via `jj config set --repo`), not a
/// per-invocation flag, so every call here can stay a plain `jj <args>`.
/// `pub` (like [`git`]) so `tests/e2e/moving_scope.rs`/`tests/e2e/timeline.rs`
/// can drive real `jj commit`/`jj util snapshot` calls *after* a harness is
/// already spawned against a fixture built here.
pub fn jj(dir: &Path, args: &[&str]) {
    let output = Command::new("jj")
        .args(["--color", "never", "--no-pager"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn jj — is it on PATH?");
    assert!(
        output.status.success(),
        "jj {args:?} failed in {}:\nstdout: {}\nstderr: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// `jj git init --colocate` can't go through [`jj`]'s `current_dir`-only
/// wrapper the way every *later* jj call in a fixture can — there's no repo
/// yet at this point for jj to detect from `dir` as a cwd, and this is the
/// one call that creates it (mirrors the identical split
/// `src/vcs/jj.rs`'s and `src/ui/timeline_view.rs`'s own real-jj test
/// fixtures both make, for the same reason — see either's
/// `jj_git_init_colocate`). Requires `dir` to already be a git repo (see
/// [`init_repo`]): `--colocate` adopts an *existing* `.git`, it doesn't
/// create one.
fn jj_git_init_colocate(dir: &Path) {
    let output = Command::new("jj")
        .args([
            "--color",
            "never",
            "--no-pager",
            "git",
            "init",
            "--colocate",
        ])
        .current_dir(dir)
        .output()
        .expect("failed to spawn jj — is it on PATH?");
    assert!(
        output.status.success(),
        "jj git init --colocate failed in {}:\nstdout: {}\nstderr: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// A colocated jj+git repo — `.jj` initialized alongside the same `.git`
/// [`init_repo`] always builds first, with repo-scoped `user.name`/
/// `user.email` set (`jj config set --repo`, never a real `~/.jjconfig`,
/// which the test process's per-test `$HOME` deliberately has none of —
/// see `support::harness::Harness::spawn`'s docs) — every jj-backed E2E
/// fixture in this file starts here, the same base every real-jj *unit*
/// test fixture (`src/vcs/jj.rs`'s `jj_fixture`, `src/ui/timeline_view.rs`'s
/// `jj_timeline_fixture`) already builds for itself.
fn init_jj_repo() -> tempfile::TempDir {
    let dir = init_repo();
    let root = dir.path();
    jj_git_init_colocate(root);
    jj(
        root,
        &["config", "set", "--repo", "user.name", "katamari e2e"],
    );
    jj(
        root,
        &["config", "set", "--repo", "user.email", "e2e@katamari.test"],
    );
    dir
}

/// A colocated jj+git repo, one real commit ("first") deep — the
/// moving-*jj*-revset counterpart to [`moving_scope_repo`]'s git-only
/// fixture, for issue #8's own "define how this interacts with... jj
/// moving revsets" acceptance criterion. `jj commit -m "first"` both
/// records "first"'s content *and* advances `@` onto a fresh, empty change
/// on top of it — so `@-` (not `@`, which starts out empty with nothing of
/// its own to diff against a not-yet-existing parent) is this fixture's
/// moving scope, the direct jj analogue of git's `HEAD` always naming the
/// tip of whatever has been committed so far. `tests/e2e/moving_scope.rs`'s
/// jj test opens `ktmr diff -r @-`, then runs a second real `jj commit`
/// itself — the same "amend after the harness is already spawned" shape
/// [`moving_scope_repo`]'s own docs describe — to advance `@-` onto a new
/// finalized change, proving the live scope follows it.
pub fn jj_moving_scope_repo() -> FixtureRepo {
    let dir = init_jj_repo();
    let root = dir.path();

    std::fs::write(root.join("notes.txt"), "alpha\nbeta\ngamma\n").unwrap();
    jj(root, &["commit", "-m", "first"]);

    FixtureRepo { dir }
}

/// A colocated jj+git repo with two live working-copy snapshots recorded
/// via `jj util snapshot` — the shape `crate::vcs::jj::JjRepo::snapshot_ops`
/// actually surfaces (see that method's docs: only operations described
/// exactly `"snapshot working copy"` count, which a `jj commit`'s
/// `describe`+`new` never produces), rebuilt here by shelling to `jj util
/// snapshot` directly since this crate has no library target an E2E binary
/// could call `JjRepo::snapshot` through — the same two-snapshot shape
/// `src/ui/timeline_view.rs`'s own `jj_timeline_fixture` unit-test helper
/// builds via that method. Never a `jj commit`: colocation auto-exports
/// `@`'s current tree into the git index on every jj command regardless
/// (confirmed empirically — `git status` shows it staged even with zero
/// real commits), and `GitSource::baseline`'s already-tested unborn-HEAD
/// fallback (the empty tree) means `ktmr diff`'s own *outer* working-tree
/// diff still shows real content with no `jj commit` needed at all. The
/// second snapshot's `SYMONE SYMTWO` line gives `tests/e2e/timeline.rs` a
/// row with two identifier-like tokens to cycle between, mirroring that
/// same unit test's own reason for the exact text.
pub fn jj_timeline_repo() -> FixtureRepo {
    let dir = init_jj_repo();
    let root = dir.path();

    std::fs::write(root.join("a.txt"), "one\n").unwrap();
    jj(root, &["util", "snapshot"]);
    std::fs::write(root.join("a.txt"), "one\nSYMONE SYMTWO\n").unwrap();
    jj(root, &["util", "snapshot"]);

    FixtureRepo { dir }
}

/// A repo whose working-tree edit touches Japanese (CJK, double-width)
/// content — the first end-to-end guard for the whole rendering pipeline
/// (parse -> highlight -> terminal cells) on wide characters, not just the
/// unit-level `is_wide`/`display_width` tests that already exist.
pub fn japanese_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    std::fs::write(
        root.join("greeting.txt"),
        "hello\nこんにちは世界\ngoodbye\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    std::fs::write(
        root.join("greeting.txt"),
        "hello\nこんにちは世界、これは日本語のテストです\ngoodbye\n",
    )
    .unwrap();

    FixtureRepo { dir }
}

/// A repo whose working-tree edit adds a single, very long content line —
/// the M13 wrap E2E fixture. The line is built to an exact, known shape:
/// 100 display columns of prefix (70 ASCII dashes then 15 double-width
/// Japanese characters, mixing both the way a real diff line would), then a
/// `TAILMARKER` word found nowhere else in the fixture. At the default
/// 100-column terminal `tests/e2e/wrap.rs`'s two tests spawn, with the
/// sidebar showing, `[ui] wrap`'s rendered content width works out to 49
/// columns (`100 - 30 sidebar - 2 border - 19 gutter` — see
/// `diff_view::unified_content_width`/`gutter_width`; issue #14 grew the
/// diff pane's border from 1 column, a bare left rule, to 2, a full
/// `PaneChrome` box — `unified_content_width` derives this from
/// `pane::inner_rect` rather than hand-counting it, so this comment is the
/// only place that number is written down at all). `TAILMARKER` (columns
/// 101-110) still lands fully intact on the third wrapped row (columns
/// 99-147) at that width — 49 no longer divides 100 evenly the way the
/// pre-#14 width of 50 did, so it's no longer aligned to that row's very
/// first column, but it still fits inside it — or never renders at all
/// when truncated: still an unambiguous, deterministic witness either way.
/// At *other* terminal widths this alignment isn't guaranteed — narrow
/// enough and `TAILMARKER` itself can land split across two wrapped rows
/// (`tests/e2e/focus.rs`'s narrow-terminal case hits exactly this and
/// checks for the wrap marker glyph instead of the word itself). `wrap` is
/// always written into `.katamari/config.toml` explicitly (never left to
/// the built-in default), so the fixture's behavior doesn't silently
/// depend on what that default happens to be.
pub fn long_line_repo(wrap: bool) -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    std::fs::write(root.join("long.txt"), "one\ntwo\nthree\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    let prefix = format!("{}{}", "-".repeat(70), "日本語のテキストで幅を確認する");
    let long_line = format!("{prefix}TAILMARKER");
    std::fs::write(root.join("long.txt"), format!("one\n{long_line}\nthree\n")).unwrap();

    std::fs::create_dir_all(root.join(".katamari")).unwrap();
    std::fs::write(
        root.join(".katamari").join("config.toml"),
        format!("[ui]\nwrap = {wrap}\n"),
    )
    .unwrap();

    FixtureRepo { dir }
}

/// A repo whose working-tree edit touches two lines far apart in one
/// 40-line file (line 3 and line 35) — far enough apart that git's default
/// 3-line unified context leaves a real, non-adjacent gap between the two
/// resulting hunks. `GAPMARKER_UNIQUE_TEXT` sits at line 20, squarely
/// inside that gap and nowhere else in the fixture, so its (dis)appearance
/// on screen is an unambiguous witness for whether the fold row is
/// expanded or collapsed. The Issue #2 fold E2E test (`tests/e2e/fold.rs`)
/// is the one thing that cares about this exact shape — see that file for
/// the row-layout arithmetic it depends on.
pub fn fold_gap_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    let mut lines: Vec<String> = (1..=40).map(|n| format!("line {n}")).collect();
    lines[19] = "GAPMARKER_UNIQUE_TEXT".to_owned();
    std::fs::write(root.join("big.txt"), lines.join("\n") + "\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    lines[2] = "line 3 CHANGED".to_owned();
    lines[34] = "line 35 CHANGED".to_owned();
    std::fs::write(root.join("big.txt"), lines.join("\n") + "\n").unwrap();

    FixtureRepo { dir }
}

/// A repo whose working-tree edit appends 60 consecutive new lines to one
/// file — all part of a single hunk (unlike [`fold_gap_repo`], nothing here
/// is far enough from a change to leave a git-omitted gap, so every line is
/// real, searchable `RenderRow::Line` content, never a fold row). One of
/// them, near the bottom of the appended block, is
/// `SEARCH_TARGET_UNIQUE` — the Issue #5 search E2E suite's witness that
/// `/` actually moved the viewport to reveal it, rather than something
/// already on screen: a 30-row terminal's diff pane shows nowhere near 63
/// rows (`FileHeader` + `HunkHeader` + 3 unchanged intro lines + 60 added
/// lines) without scrolling.
pub fn search_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    std::fs::write(
        root.join("search.txt"),
        "intro line one\nintro line two\nintro line three\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    let mut appended: Vec<String> = (1..=60).map(|n| format!("filler line {n}")).collect();
    appended[54] = "SEARCH_TARGET_UNIQUE appears here".to_owned();
    let content = format!(
        "intro line one\nintro line two\nintro line three\n{}\n",
        appended.join("\n")
    );
    std::fs::write(root.join("search.txt"), content).unwrap();

    FixtureRepo { dir }
}

/// A repo whose sole working-tree edit adds `main.stub` — an extension
/// nothing built into katamari has ever heard of — paired with a
/// `.katamari/config.toml` `[lsp.servers.stubls]` entry that claims `.stub`
/// and points at `support::fake_lsp_server`'s script (see the module docs
/// above for why this is the one fixture allowed to make katamari actually
/// spawn something). The committed and working-tree content share the
/// first two lines (`alpha`/`beta`, both `Context` rows once diffed) with
/// `GOTO_TARGET_TOKEN` added as a third — any of the three is a valid
/// hover/goto target (katamari's `App::hover_query` accepts `Context` and
/// `Add` rows alike), so a test can press an action from wherever `j`/`k`
/// lands the cursor without needing to know the diff's exact row layout.
///
/// `init_delay_secs` is how long the fake server waits before answering
/// `initialize` — the window a test presses actions during to prove they
/// report "not ready" instead of queuing. `definition_delay_secs` is how
/// long it waits before answering `textDocument/definition` once `Ready` —
/// for a test that wants to prove movement stays responsive while a
/// *ready* server is still slow to answer the request itself.
/// Whether the fake-LSP fixture can run at all: `fake_lsp_server.py` needs
/// a `python3` on `$PATH`, the one external dependency this otherwise
/// hermetic support module has (see the module doc). Tests that spawn
/// [`lsp_readiness_repo`] should skip (with an eprintln, so the skip is
/// visible in `--nocapture` runs) rather than fail when it's missing —
/// a contributor without python3 would otherwise see the spawn collapse
/// into `Unavailable: command not found` and the test time out waiting
/// for "is starting" text that can never appear, which reads like a
/// katamari regression instead of a missing tool.
pub fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

/// A repo shaped for issue #15's file-tree PTY coverage: a changed file
/// nested at least two directories deep, a deleted file, a renamed file,
/// and a large padding file placed alphabetically (and therefore
/// diff-order) first. `tests/e2e/file_tree.rs` spawns `ktmr diff --staged`
/// against this fixture, not the plain working-tree default: katamari's own
/// `GitSource::working_tree_diff` combines plain `git diff` (tracked,
/// unstaged changes) with a *separate* `--no-index` diff per untracked
/// file, so an unstaged rename (an untracked new path plus a missing
/// tracked one) can never pair up into one `rename from`/`rename to` entry
/// the way a *staged* rename does — only `git diff --cached` (what
/// `--staged` shows) ever produces one.
///
/// `src/aaa_padding.txt`'s only job is bulk: 60 added lines, sorted (and
/// therefore diffed) ahead of everything else, so the nested marker file's
/// own `FileHeader` row lands well below a real terminal's initial diff-pane
/// viewport at the default `SpawnOptions` size. That's what makes
/// "`NESTED_MARKER_UNIQUE` leaves the screen when its directory collapses"
/// an unambiguous witness for the *sidebar's* own collapse state rather
/// than a coincidence of whatever the diff pane happens to have scrolled to
/// — collapsing a tree row never changes the diff pane's own content, only
/// the sidebar's.
pub fn tree_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();
    let src = root.join("src");
    let nested = src.join("nested").join("deep");
    std::fs::create_dir_all(&nested).unwrap();

    std::fs::write(src.join("doomed.txt"), "going away soon\n").unwrap();
    std::fs::write(
        src.join("old_name.txt"),
        "renamed content\nline two\nline three\n",
    )
    .unwrap();
    std::fs::write(nested.join("NESTED_MARKER_UNIQUE.txt"), "before\nafter\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    let padding: String = (1..=60).map(|n| format!("padding line {n}\n")).collect();
    std::fs::write(src.join("aaa_padding.txt"), padding).unwrap();

    std::fs::remove_file(src.join("doomed.txt")).unwrap();

    std::fs::write(
        nested.join("NESTED_MARKER_UNIQUE.txt"),
        "before\nMARKER_CONTENT_LINE_UNIQUE\nafter\n",
    )
    .unwrap();

    std::fs::rename(src.join("old_name.txt"), src.join("new_name.txt")).unwrap();

    // Everything above must be staged — see this function's own docs on
    // why a rename only survives as one diff entry once it's in the index.
    git(root, &["add", "-A"]);

    FixtureRepo { dir }
}

/// `count` small top-level files, each with one changed line — issue #20's
/// mouse E2E fixture. Two independent things need to overflow a default
/// (100x30) terminal at once here: the *sidebar*, so wheel-scrolling it is
/// observable (`count` flat top-level files means `count` sidebar rows, no
/// synthetic directory row to pad past the terminal's usable ~26 with), and
/// the *diff pane*, so wheel-scrolling it moves real content into view
/// (`count` files at ~5 rendered rows each — file header, hunk header,
/// context/changed/context — comfortably clears a 30-row screen well
/// before `count` on its own would). `FIRST_MARKER`/`LAST_MARKER` sit in
/// the first and last files respectively (alphabetical == diff order here,
/// since every name is zero-padded to sort correctly) — unambiguous
/// witnesses for "did the diff pane's visible window actually move," the
/// same role `SEARCH_TARGET_UNIQUE` plays in [`search_repo`].
pub fn many_files_repo(count: usize) -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    for i in 0..count {
        std::fs::write(root.join(format!("file{i:03}.txt")), "alpha\nbeta\ngamma\n").unwrap();
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    for i in 0..count {
        let marker = match i {
            0 => "FIRST_MARKER",
            n if n == count - 1 => "LAST_MARKER",
            _ => "CHANGED",
        };
        std::fs::write(
            root.join(format!("file{i:03}.txt")),
            format!("alpha {marker}\nbeta\ngamma\n"),
        )
        .unwrap();
    }

    FixtureRepo { dir }
}

pub fn lsp_readiness_repo(init_delay_secs: f64, definition_delay_secs: f64) -> FixtureRepo {
    lsp_readiness_repo_inner(init_delay_secs, definition_delay_secs, "0")
}

/// As [`lsp_readiness_repo`], but `textDocument/definition` answers with a
/// real [`lsp_types::Location`] in a second, unmodified file (`other.stub`)
/// instead of `None` — issue #12's Esc-from-a-definition-opened-`FileView`
/// PTY coverage needs a genuine `FileView` push, which a `null` result (or
/// a location inside `main.stub` itself, already part of this diff — see
/// `App::row_for_target`) can't produce. Kept as a separate function rather
/// than a third parameter every existing caller would have to thread
/// through: issue #11's not-ready/no-result assertions never have to think
/// about this at all.
///
/// [`lsp_types::Location`]: https://docs.rs/lsp-types/latest/lsp_types/struct.Location.html
pub fn lsp_readiness_repo_with_definition_target(
    init_delay_secs: f64,
    definition_delay_secs: f64,
) -> FixtureRepo {
    lsp_readiness_repo_inner(init_delay_secs, definition_delay_secs, "1")
}

/// As [`lsp_readiness_repo_with_definition_target`], but the definition
/// answer is a *two*-`Location` array — the only response shape that makes
/// the client open its "Definitions" `RefsPanel` instead of navigating
/// (see `ui::apply_definition_result`), which the panel's own
/// close-and-consume mouse coverage needs and no other fixture mode can
/// reach.
pub fn lsp_readiness_repo_with_two_definition_targets(
    init_delay_secs: f64,
    definition_delay_secs: f64,
) -> FixtureRepo {
    lsp_readiness_repo_inner(init_delay_secs, definition_delay_secs, "2")
}

fn lsp_readiness_repo_inner(
    init_delay_secs: f64,
    definition_delay_secs: f64,
    definition_mode: &str,
) -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    std::fs::write(root.join("main.stub"), "alpha\nbeta\n").unwrap();
    // Committed once, never touched again — absent from the working-tree
    // diff `main.stub`'s own edit below produces, so a jump into it can
    // only ever be a real navigation target, never "coincidentally already
    // part of this diff" (see `lsp_readiness_repo_with_definition_target`'s
    // docs).
    std::fs::write(
        root.join("other.stub"),
        "target line one\ntarget line two\n",
    )
    .unwrap();
    // `.katamari/config.toml` is committed alongside the two `.stub` files
    // rather than left untracked (issue #26): a directory always sorts
    // ahead of a sibling file in canonical (tree) order regardless of
    // name, so an untracked `.katamari/` would land *ahead* of `main.stub`
    // in the diff pane — every test below navigates to `main.stub`'s own
    // added row by a fixed number of keypresses from the top, an
    // assumption a config file jumping the queue would break for no
    // reason relevant to what these tests actually cover (LSP readiness,
    // not the sidebar/diff-pane's file ordering — that's issue #26's own
    // `file_tree`/`app` coverage). Committing it here keeps it out of the
    // working-tree diff entirely, exactly like `other.stub` above.
    let server_script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/fake_lsp_server.py"
    );
    std::fs::create_dir_all(root.join(".katamari")).unwrap();
    std::fs::write(
        root.join(".katamari").join("config.toml"),
        format!(
            "[lsp.servers.stubls]\n\
             command = \"python3\"\n\
             args = [\"{server_script}\", \"{init_delay_secs}\", \"{definition_delay_secs}\", \"{definition_mode}\"]\n\
             extensions = [\"stub\"]\n",
        ),
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    std::fs::write(root.join("main.stub"), "alpha\nbeta\nGOTO_TARGET_TOKEN\n").unwrap();

    FixtureRepo { dir }
}

/// A bare "upstream" repository with `HEAD` pointed at `main` — deliberately
/// via `git init -b main --bare` rather than a plain `--bare` init followed
/// by a `symbolic-ref` fixup: a freshly `--bare`-init'd repo otherwise
/// defaults to `refs/heads/master` (confirmed empirically), and that default
/// would stick permanently once the first push landed on it under the wrong
/// name. `dir` receives the clone (the `FixtureRepo` a test actually opens
/// `ktmr diff` against); `upstream`/`seed` are dropped once this returns —
/// `git clone` already copied every object it needs and wrote
/// `refs/remotes/origin/HEAD`/`refs/remotes/origin/main` as *local* refs, so
/// nothing later ever touches either directory again (verified by hand
/// while building this fixture: `git symbolic-ref -q --short
/// refs/remotes/origin/HEAD` still resolves correctly from the clone alone).
fn clone_of_a_freshly_pushed_main(root: &Path) {
    let upstream = tempfile::Builder::new()
        .prefix("katamari-e2e-upstream-")
        .tempdir()
        .expect("failed to create fixture tempdir");
    git(upstream.path(), &["init", "-q", "-b", "main", "--bare"]);

    let seed = tempfile::Builder::new()
        .prefix("katamari-e2e-seed-")
        .tempdir()
        .expect("failed to create fixture tempdir");
    git(seed.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(seed.path().join("notes.txt"), "alpha\nbeta\ngamma\n").unwrap();
    git(seed.path(), &["add", "-A"]);
    git(seed.path(), &["commit", "-q", "-m", "first"]);
    git(
        seed.path(),
        &["remote", "add", "origin", upstream.path().to_str().unwrap()],
    );
    git(seed.path(), &["push", "-q", "origin", "main"]);

    let status = Command::new("git")
        .args(["clone", "-q"])
        .arg(upstream.path())
        .arg(root)
        .status()
        .expect("git must be on PATH for these tests");
    assert!(status.success(), "git clone failed into {}", root.display());
}

/// [`clone_of_a_freshly_pushed_main`]'s clone, with a `feature` branch
/// checked out two real commits ahead of `main` — `tests/e2e/branch_scope.rs`'s
/// main fixture for `--branch`/the scope-menu's "Branch vs base" entry/the
/// `B` keybinding. Clean working tree throughout, the same
/// dirty-tree-could-let-`git add -A`-sweep-in-something-unintended reasoning
/// [`clean_repo`]'s own docs give: every edit below is a real commit, never
/// left uncommitted. `FEATURE_MARKER_ONE`/`FEATURE_MARKER_TWO` exist only on
/// `feature`, one per commit — unambiguous witnesses for "is the branch-vs-base
/// diff (not `main`'s own empty diff against itself) actually on screen,"
/// and for telling the two commits' own content apart when a test needs to.
pub fn branch_ahead_of_main_repo() -> FixtureRepo {
    let dir = tempfile::Builder::new()
        .prefix("katamari-e2e-repo-")
        .tempdir()
        .expect("failed to create fixture tempdir");
    clone_of_a_freshly_pushed_main(dir.path());

    let root = dir.path();
    git(root, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(
        root.join("notes.txt"),
        "alpha\nbeta\ngamma\nFEATURE_MARKER_ONE\n",
    )
    .unwrap();
    git(root, &["commit", "-q", "-am", "feature commit one"]);
    std::fs::write(
        root.join("notes.txt"),
        "alpha\nbeta\ngamma\nFEATURE_MARKER_ONE\nFEATURE_MARKER_TWO\n",
    )
    .unwrap();
    git(root, &["commit", "-q", "-am", "feature commit two"]);

    FixtureRepo { dir }
}

/// [`branch_ahead_of_main_repo`]'s jj-colocated counterpart: the same clone
/// (`main` pushed to a real "upstream", `origin/HEAD` auto-set) with `jj git
/// init --colocate` layered on top and a `feature` bookmark advanced one
/// real `jj commit` past `trunk()` — `jj git init --colocate` auto-detects
/// and pins the `trunk()` revset alias to `main@origin` at init time in
/// exactly this shape (empirically confirmed while building
/// `vcs::jj::JjRepo::trunk_bookmarks`'s own unit tests: it prints "Setting
/// the revset alias `trunk()` to `main@origin`"), so no extra `jj bookmark
/// track` step is needed here the way an untracked-remote-bookmark repo
/// would require. `jj_available()`'s own docs explain why every caller must
/// skip (not fail) when `jj` isn't on `PATH`.
pub fn jj_branch_ahead_of_trunk_repo() -> FixtureRepo {
    let dir = tempfile::Builder::new()
        .prefix("katamari-e2e-repo-")
        .tempdir()
        .expect("failed to create fixture tempdir");
    clone_of_a_freshly_pushed_main(dir.path());
    let root = dir.path();

    jj_git_init_colocate(root);
    jj(
        root,
        &["config", "set", "--repo", "user.name", "katamari e2e"],
    );
    jj(
        root,
        &["config", "set", "--repo", "user.email", "e2e@katamari.test"],
    );
    std::fs::write(
        root.join("notes.txt"),
        "alpha\nbeta\ngamma\nJJ_FEATURE_MARKER\n",
    )
    .unwrap();
    jj(root, &["commit", "-m", "feature commit"]);
    jj(root, &["bookmark", "set", "feature", "-r", "@-"]);

    FixtureRepo { dir }
}
