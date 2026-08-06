//! M12's scope-picker popup (`o` from a live `ktmr diff` session): opening
//! the menu, swapping the current diff to a typed revision, and swapping
//! back to the working tree — end to end, through the real compiled binary
//! and a real `git` subprocess, not just `App`'s in-process state
//! transitions [`crate::support`]'s harness normally covers via
//! `ui::scope_menu`'s own unit tests.

use crate::support::harness::DEFAULT_WAIT;
use crate::support::{Harness, Key, SpawnOptions, fixture};

#[test]
fn scope_menu_swaps_to_a_typed_revision_and_back_to_the_working_tree() {
    // `basic_repo`: one commit ("initial commit", which adds README.md and
    // notes.txt) plus uncommitted working-tree edits (README.md changed,
    // todo.txt added) — the same fixture `log.rs`'s E2E test uses, and for
    // the same reason: `notes.txt`/`alpha` only ever appear in the commit's
    // own diff, `todo.txt` only ever appears in the working-tree diff, so
    // which one's on screen unambiguously proves which scope is active.
    let repo = fixture::basic_repo();
    // Commit hashes aren't deterministic across runs — ask the fixture for
    // the one it actually produced rather than hardcoding anything.
    let head_hash = repo.commit_hash("HEAD");

    let h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff"],
            ..Default::default()
        },
    );

    // Starts on the dirty working tree, same as a plain `ktmr diff`.
    h.wait_for_text("todo.txt");

    // `o` opens the scope-menu popup. No jj repo here (git-only fixture),
    // so the menu's entries are exactly Working tree / Staged / Log /
    // Revision… — `Timeline (jj)` is absent (see
    // `crate::ui::scope_menu::available_entries`'s docs).
    h.send(Key::Char('o'));
    h.wait_for_text("Working tree");
    h.wait_for_text("Revision");
    assert!(
        !h.screen_contents().contains("Timeline"),
        "no colocated jj repo in this fixture — Timeline must not be offered"
    );

    // Esc closes the popup without swapping anything — back to the same
    // dirty working-tree diff, menu gone. `todo.txt` is already visible in
    // the sidebar behind the popup the whole time, so waiting for it to
    // appear proves nothing about whether Esc was actually processed yet —
    // wait for the popup's own text to actually disappear instead of
    // racing a fixed sleep against it.
    h.send(Key::Esc);
    h.wait_until(DEFAULT_WAIT, |screen| {
        !screen.contents().contains("Revision")
    });

    // Reopen it for the real run: down three times: Working tree -> Staged
    // -> Log -> Revision…
    h.send(Key::Char('o'));
    h.wait_for_text("Revision");
    for _ in 0..3 {
        h.send(Key::Char('j'));
    }
    h.send(Key::Enter);

    // The one-line revision input is now open — its prompt names what a
    // git-only repo accepts.
    h.wait_for_text("git rev");

    for c in head_hash.chars() {
        h.send(Key::Char(c));
    }
    h.send(Key::Enter);

    // Swapped to the commit's own diff: its content is on screen, tagged
    // with the scope label M11 already established for a revision diff
    // (`App::scope_label`'s `Some("r: <id>")` form — see `status_bar::render`).
    h.wait_for_text("notes.txt");
    h.wait_for_text("alpha");
    h.wait_for_text(&format!("r: {head_hash}"));

    // Reopen the menu and swap back to the working tree — the first entry,
    // so no navigation needed before `Enter`.
    h.send(Key::Char('o'));
    h.wait_for_text("Working tree");
    h.send(Key::Enter);

    // Back to the dirty working tree: `todo.txt` (only ever in the
    // working-tree diff) is visible again, and the revision scope label is
    // gone.
    h.wait_for_text("todo.txt");
    assert!(
        !h.screen_contents().contains(&format!("r: {head_hash}")),
        "the revision scope label must clear once swapped back to the working tree"
    );
}

#[test]
fn revision_menu_reports_an_invalid_revision_without_blanking_the_diff() {
    let repo = fixture::basic_repo();
    let h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff"],
            ..Default::default()
        },
    );
    h.wait_for_text("todo.txt");

    h.send(Key::Char('o'));
    h.wait_for_text("Revision");
    for _ in 0..3 {
        h.send(Key::Char('j'));
    }
    h.send(Key::Enter);
    h.wait_for_text("git rev");

    for c in "not-a-real-revision".chars() {
        h.send(Key::Char(c));
    }
    h.send(Key::Enter);

    // git's own error surfaces as a status-bar note; the dirty working-tree
    // diff that was already on screen stays exactly as it was — never a
    // blank pane, never a crash.
    h.wait_for_text("scope:");
    h.wait_for_text("todo.txt");
}
