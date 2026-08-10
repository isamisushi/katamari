//! `ktmr log`'s own E2E coverage, git-only backend (no `jj` dependency, so
//! this runs anywhere the rest of the suite does): the synthetic
//! "local changes" row for a dirty working tree, opening a real commit's
//! diff on `Enter`, `Esc` popping exactly the pushed diff back to the log
//! list, and `q` quitting the whole session immediately — even from that
//! pushed diff, never "back" to the list underneath it (issue #12; this
//! file used to assert the opposite, pre-#12, behavior — see git history).

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn esc_pops_a_pushed_diff_back_to_the_log_list() {
    // `basic_repo` is exactly the fixture this test needs off the shelf:
    // one commit ("initial commit") plus uncommitted working-tree edits —
    // a dirty tree, so the synthetic local-changes row exists and sorts
    // first.
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["log"],
            ..Default::default()
        },
    );

    h.wait_for_text("local changes");
    h.wait_for_text("initial commit");

    // Enter on the local-changes row (the default selection) opens the same
    // working-tree diff a plain `ktmr diff` would.
    h.send(Key::Enter);
    h.wait_for_text("README.md");
    // `todo.txt` is a brand new untracked file in the working tree — it has
    // no committed history, so it can only ever show up in this
    // working-tree diff, never in the commit diff exercised below.
    h.wait_for_text("todo.txt");

    // `Esc` pops exactly the pushed diff, landing back on the log list —
    // never quitting the session (that's `q`'s job now, checked below).
    h.send(Key::Esc);
    h.wait_for_text("local changes");
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("todo.txt")
    });

    // Move down to the one commit row, open its diff, and pop back the same
    // way a second time — `Esc` popping isn't a one-shot fluke.
    h.send(Key::Char('j'));
    h.send(Key::Enter);
    // `notes.txt` was created and fully populated in "initial commit" and
    // never touched again — present only in that commit's own diff (an
    // all-add root-commit diff), never in the working-tree diff above.
    h.wait_for_text("notes.txt");
    h.wait_for_text("alpha");

    h.send(Key::Esc);
    h.wait_for_text("local changes");
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("notes.txt")
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// The other direction of the pushed-view pop: not a diff pushed onto a
/// root log, but a `LogView` pushed onto the root *diff* with `L` — the
/// arm of `cancel_diff_view` the test above can't reach (its root is the
/// log itself). `TimelineView` shares this exact match arm but needs a jj
/// repo to open at all, so the git-backed `LogView` stands in for both.
#[test]
fn esc_pops_an_l_pushed_log_view_back_to_the_root_diff() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("README.md");
    h.wait_for_text("todo.txt");

    h.send(Key::Char('L'));
    h.wait_for_text("local changes");
    h.wait_for_text("initial commit");

    // `Esc` pops exactly the pushed log, revealing the root diff again —
    // never quitting, never clearing anything on the diff underneath.
    h.send(Key::Esc);
    h.wait_for_text("todo.txt");
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("initial commit")
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn q_quits_the_session_immediately_from_a_pushed_diff_rather_than_returning_to_the_log() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["log"],
            ..Default::default()
        },
    );

    h.wait_for_text("local changes");
    h.send(Key::Enter);
    h.wait_for_text("todo.txt");

    // `q` from the pushed diff exits the whole process outright — the exact
    // bug this issue fixes: before #12, `q` here behaved like `Esc`,
    // popping back to the log list instead of quitting. `wait_exit` timing
    // out is exactly what that regression would look like.
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(
        status.success(),
        "ktmr should exit 0 on q from a pushed diff, got {status:?}"
    );
}

#[test]
fn q_quits_the_session_directly_from_the_log_views_own_root() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["log"],
            ..Default::default()
        },
    );

    h.wait_for_text("local changes");
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(
        status.success(),
        "ktmr should exit 0 on q from the log view's own root, got {status:?}"
    );
}
