//! Throwaway git repositories for the E2E suite to point `ktmr diff` at.
//! Every fixture uses plain `.txt`/`.md` content deliberately — never
//! `.rs`/`.ts`/`.py`/`.go` — so [`katamari`'s `Language::detect`] never
//! matches, which means [`LspManager::warm_up`] never has anything eligible
//! to open and no language server is ever resolved, installed, or spawned.
//! An E2E suite that accidentally depended on network access (an
//! auto-install) or a locally installed toolchain would be flaky in exactly
//! the ways this milestone is trying to eliminate.

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
fn git(dir: &Path, args: &[&str]) {
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
/// `TAILMARKER` word found nowhere else in the fixture. At a 100-column
/// terminal with the sidebar showing, `[ui] wrap`'s rendered content width
/// works out to 50 columns (`100 - 30 sidebar - 1 border - 19 gutter` — see
/// `diff_view::unified_content_width`/`gutter_width`), so `TAILMARKER`
/// (starting at column 100) lands intact on a continuation row when
/// wrapped, or never renders at all when truncated — an unambiguous,
/// deterministic witness either way. `wrap` is always written into
/// `.katamari/config.toml` explicitly (never left to the built-in default),
/// so the fixture's behavior doesn't silently depend on what that default
/// happens to be.
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
