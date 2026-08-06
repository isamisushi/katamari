//! `ktmr log`'s own E2E coverage, git-only backend (no `jj` dependency, so
//! this runs anywhere the rest of the suite does): the synthetic
//! "local changes" row for a dirty working tree, opening a real commit's
//! diff on `Enter`, and `q` popping back to the list rather than quitting
//! the whole session — the same "push a view, `q` pops back" shape
//! goto-definition already exercises for `FileView`, now for
//! `LogView`-opened diffs too.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn log_local_changes_row_then_a_commit_diff_both_open_and_pop_back_on_q() {
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

    // `q` closes the pushed diff, landing back on the log list — not
    // quitting the whole session.
    h.send(Key::Char('q'));
    h.wait_for_text("local changes");

    // Move down to the one commit row and open its diff.
    h.send(Key::Char('j'));
    h.send(Key::Enter);
    // `notes.txt` was created and fully populated in "initial commit" and
    // never touched again — present only in that commit's own diff (an
    // all-add root-commit diff), never in the working-tree diff above.
    h.wait_for_text("notes.txt");
    h.wait_for_text("alpha");

    h.send(Key::Char('q')); // back to the log list again
    h.wait_for_text("local changes");
    h.send(Key::Char('q')); // quit the session (log view is the root here)
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
