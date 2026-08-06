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
