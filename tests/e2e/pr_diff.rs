//! `ktmr diff --pr` against a fake `gh` on PATH: proves the spawn/capture
//! plumbing, that the headless render includes GitHub's default
//! git-quoted paths flowing through the decoder, that `gh`'s own stderr
//! surfaces on failure, and the install hint when no `gh` exists at all —
//! no network, no real GitHub repository. The same shadow-a-CLI-on-PATH
//! pattern as `support/fake_lsp_server.py`, just small enough to inline.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// Writes an executable `gh` into `dir`; prepending `dir` to PATH makes
/// it shadow any real installation.
fn write_fake_gh(dir: &Path, script_body: &str) {
    let path = dir.join("gh");
    std::fs::write(&path, format!("#!/bin/sh\n{script_body}\n")).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

fn path_with(dir: &Path) -> std::ffi::OsString {
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    std::env::join_paths(paths).unwrap()
}

#[test]
fn pr_diff_renders_the_fake_ghs_quoted_path_diff_headlessly() {
    let repo = fixture::basic_repo();
    let gh_dir = tempfile::tempdir().unwrap();
    // A café.txt rename target exercises the git-quoted (octal-escaped)
    // path headers GitHub serves with its default core.quotepath — the
    // decoding this feature adopted from PR #25.
    write_fake_gh(
        gh_dir.path(),
        r#"echo "$@" > gh-args.txt
cat <<'EOF'
diff --git "a/caf\303\251.txt" "b/caf\303\251.txt"
index 0000000..1111111 100644
--- "a/caf\303\251.txt"
+++ "b/caf\303\251.txt"
@@ -1 +1,2 @@
 old line
+new line for the pull request
EOF"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .current_dir(repo.path())
        .env("PATH", path_with(gh_dir.path()))
        .args(["diff", "--pr", "7", "--dump"])
        .output()
        .expect("spawn ktmr");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("café.txt"), "{stdout}");
    assert!(stdout.contains("new line for the pull request"), "{stdout}");

    // The fake ran from the repo root with exactly the arguments the
    // real `gh` would need — the whole contract of the spawn.
    let args = std::fs::read_to_string(repo.path().join("gh-args.txt")).unwrap();
    assert_eq!(args.trim(), "pr diff 7");
}

#[test]
fn a_pr_diff_opens_in_the_real_tui_labeled_and_read_only() {
    let repo = fixture::basic_repo();
    let gh_dir = tempfile::tempdir().unwrap();
    // The rendered content must come from the fake gh's snapshot, not
    // from anything in the fixture repo — the marker line exists nowhere
    // else, so seeing it on screen proves the PR text is what rendered.
    write_fake_gh(
        gh_dir.path(),
        r#"cat <<'EOF'
diff --git a/notes.txt b/notes.txt
index 0000000..1111111 100644
--- a/notes.txt
+++ b/notes.txt
@@ -1 +1,2 @@
 alpha
+PR_ONLY_MARKER_LINE from the pull request
EOF"#,
    );

    let h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff", "--pr", "25"],
            extra_env: vec![("PATH".into(), path_with(gh_dir.path()))],
            ..Default::default()
        },
    );

    // The status bar names the scope, and the snapshot's own content is
    // what's on screen.
    h.wait_for_text("PR #25");
    h.wait_for_text("PR_ONLY_MARKER_LINE");

    // Read-only: a comment attempt is refused with the standard
    // historical-diff note instead of opening the compose overlay.
    h.send(Key::Char('c'));
    h.wait_for_text("not available on a historical/read-only diff");
}

#[test]
fn the_scope_menu_switches_to_a_pr_and_back_mid_session() {
    let repo = fixture::basic_repo();
    let gh_dir = tempfile::tempdir().unwrap();
    write_fake_gh(
        gh_dir.path(),
        r#"cat <<'EOF'
diff --git a/notes.txt b/notes.txt
index 0000000..1111111 100644
--- a/notes.txt
+++ b/notes.txt
@@ -1 +1,2 @@
 alpha
+PR_ONLY_MARKER_LINE from the pull request
EOF"#,
    );

    let h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff"],
            extra_env: vec![("PATH".into(), path_with(gh_dir.path()))],
            ..Default::default()
        },
    );
    h.wait_for_text("This is line two, updated.");

    // `o` → the menu lists the PR entry; without jj the order is
    // Working tree / Staged / Log / Revision… / GitHub PR…, so four
    // downs land on it.
    h.send(Key::Char('o'));
    h.wait_for_text("GitHub PR…");
    for _ in 0..4 {
        h.send(Key::Char('j'));
    }
    h.send(Key::Enter);
    h.wait_for_text("scope: github pr");

    for c in ['2', '5'] {
        h.send(Key::Char(c));
    }
    h.send(Key::Enter);

    // The fetch is asynchronous; the label and the snapshot's own
    // content arriving proves the background hand-off worked end to end.
    h.wait_for_text("PR #25");
    h.wait_for_text("PR_ONLY_MARKER_LINE");

    // And back: reopening the menu starts on Working tree, and confirming
    // it restores the live diff.
    h.send(Key::Char('o'));
    h.wait_for_text("Working tree");
    h.send(Key::Enter);
    h.wait_for_text("This is line two, updated.");
}

#[test]
fn a_pr_fetch_that_lands_off_view_parks_until_its_view_returns() {
    let repo = fixture::basic_repo();
    let gh_dir = tempfile::tempdir().unwrap();
    // The sleep holds the fetch open long enough to navigate away from
    // the diff that requested it before the result lands.
    write_fake_gh(
        gh_dir.path(),
        r#"sleep 1
cat <<'EOF'
diff --git a/notes.txt b/notes.txt
index 0000000..1111111 100644
--- a/notes.txt
+++ b/notes.txt
@@ -1 +1,2 @@
 alpha
+PR_ONLY_MARKER_LINE from the pull request
EOF"#,
    );

    let h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff"],
            extra_env: vec![("PATH".into(), path_with(gh_dir.path()))],
            ..Default::default()
        },
    );
    h.wait_for_text("This is line two, updated.");

    h.send(Key::Char('o'));
    h.wait_for_text("GitHub PR…");
    for _ in 0..4 {
        h.send(Key::Char('j'));
    }
    h.send(Key::Enter);
    h.wait_for_text("scope: github pr");
    for c in ['4', '2'] {
        h.send(Key::Char(c));
    }
    h.send(Key::Enter);

    // Leave the requesting diff before the fetch lands: the log view is
    // on top when the result arrives, so it must NOT be applied there —
    // it parks, and the log view keeps showing its own content.
    h.send(Key::Char('L'));
    h.wait_for_text("local changes");

    // The fake's latency is a fixture-controlled constant (sleep 1), so
    // three times that bounds the arrival deterministically; the parked
    // result is invisible by design while the log view is up, so there
    // is nothing on screen to wait for until we return.
    std::thread::sleep(std::time::Duration::from_secs(3));
    let screen_before = h.screen_contents();
    assert!(
        !screen_before.contains("PR_ONLY_MARKER_LINE"),
        "the parked PR text must not leak into the log view:\n{screen_before}"
    );

    // Returning to the diff that asked applies the parked result — the
    // reviewer never has to retype the request.
    h.send(Key::Esc);
    h.wait_for_text("PR #42");
    h.wait_for_text("PR_ONLY_MARKER_LINE");
}

#[test]
fn pr_diff_surfaces_ghs_own_error_text() {
    let repo = fixture::basic_repo();
    let gh_dir = tempfile::tempdir().unwrap();
    write_fake_gh(
        gh_dir.path(),
        r#"echo 'GraphQL: Could not resolve to a PullRequest with the number of 999.' >&2
exit 1"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .current_dir(repo.path())
        .env("PATH", path_with(gh_dir.path()))
        .args(["diff", "--pr", "999", "--dump"])
        .output()
        .expect("spawn ktmr");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    // gh's message is the actionable part and must come through verbatim.
    assert!(
        stderr.contains("Could not resolve to a PullRequest"),
        "{stderr}"
    );
}

#[test]
fn pr_diff_without_gh_anywhere_reports_the_install_hint() {
    let repo = fixture::basic_repo();
    // A PATH holding only git (which ktmr itself needs to open the repo)
    // and sh — and no gh.
    let bin_dir = tempfile::tempdir().unwrap();
    for tool in ["git", "sh"] {
        let real = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|d| d.join(tool))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("{tool} not found on PATH"));
        std::os::unix::fs::symlink(real, bin_dir.path().join(tool)).unwrap();
    }

    let output = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .current_dir(repo.path())
        .env("PATH", bin_dir.path())
        .args(["diff", "--pr", "7", "--dump"])
        .output()
        .expect("spawn ktmr");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("https://cli.github.com"), "{stderr}");
    assert!(stderr.contains("gh auth login"), "{stderr}");
}
