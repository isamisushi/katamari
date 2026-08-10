//! Issue #19: `c` comments a valid visual selection as one range —
//! end-to-end through the real compiled binary, the same way issue #16's
//! own visual mode (`tests/e2e/visual.rs`) and #17's yank (`tests/e2e/yank.rs`)
//! are. `basic_repo`'s `README.md` rows (see those two files' own docs for
//! the same layout, reused here):
//!
//! ```text
//! 0  FileHeader
//! 1  HunkHeader
//! 2  ctx  new1  "# Sample project"
//! 3  ctx  new2  ""
//! 4  del        "This is line two."
//! 5  add  new3  "This is line two, updated."
//! 6  ctx  new4  "This is line three."
//! 7  ctx  new5  "This is line four."
//! 8  add  new6  "A brand new line five."
//! ```

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn selecting_two_context_lines_and_saving_creates_one_range_comment() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 120,
            ..Default::default()
        },
    );

    h.wait_for_text("README.md");
    // Six `j` presses from row 0 land on row 6, "This is line three."
    // (new_line 4) — see this file's own docs for the row layout.
    for _ in 0..6 {
        h.send(Key::Char('j'));
    }
    h.wait_for_text("\u{b7} 7/");

    h.send(Key::Char('V'));
    h.wait_for_text("VISUAL");
    // One more `j` extends onto row 7, "This is line four." (new_line 5) —
    // both context rows, contiguous, one file: a valid range.
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 8/");

    h.send(Key::Char('c'));
    h.wait_for_text("README.md:4-5");

    for c in "looks good".chars() {
        h.send(Key::Char(c));
    }
    h.send(Key::CtrlS);
    h.wait_for_text("comment: saved");

    let contents = h.screen_contents();
    assert!(
        !contents.contains("VISUAL"),
        "a successful save must clear the selection; screen:\n{contents}"
    );
    let three_row = contents
        .lines()
        .find(|l| l.contains("This is line three."))
        .expect("range start row");
    let four_row = contents
        .lines()
        .find(|l| l.contains("This is line four."))
        .expect("range end row");
    assert!(
        three_row.contains('\u{25C6}'),
        "the range's start row must carry the open marker: {three_row:?}"
    );
    assert!(
        four_row.contains('\u{25C6}'),
        "the range's end row must carry the open marker too: {four_row:?}"
    );
    // The comment body renders once, under the range's first line — not
    // duplicated under both.
    assert_eq!(
        contents.matches("looks good").count(),
        1,
        "the saved comment's body must appear exactly once; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn selecting_across_a_deletion_is_refused_and_keeps_the_selection() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("README.md");
    // Four `j` presses land on row 4, the deleted "This is line two." row.
    for _ in 0..4 {
        h.send(Key::Char('j'));
    }
    h.wait_for_text("\u{b7} 5/");

    h.send(Key::Char('V'));
    h.wait_for_text("VISUAL");
    // Extend onto row 5, the replacement add row — the selection now spans
    // a deletion and its replacement, which the range rules always refuse.
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 6/");

    h.send(Key::Char('c'));
    h.wait_for_text("comment: selection includes a deleted line");

    let contents = h.screen_contents();
    assert!(
        contents.contains("VISUAL"),
        "a rejected range must never clear the selection; screen:\n{contents}"
    );
    assert!(
        !contents.contains("C-s save"),
        "a rejected range must never open the compose overlay; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn cancelling_compose_on_a_valid_range_leaves_the_same_selection_in_place() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("README.md");
    for _ in 0..6 {
        h.send(Key::Char('j'));
    }
    h.wait_for_text("\u{b7} 7/");

    h.send(Key::Char('V'));
    h.wait_for_text("VISUAL");
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 8/");

    h.send(Key::Char('c'));
    h.wait_for_text("README.md:4-5");

    // Esc while composing cancels the overlay without ever reaching the
    // diff view's own Esc handling — the selection must survive untouched
    // (req 6).
    h.send(Key::Esc);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("C-s save")
    });
    h.wait_for_text("VISUAL");
    assert!(
        h.screen_contents().contains("\u{b7} 8/"),
        "cancelling compose must not move the cursor"
    );

    // Re-pressing `c` against the same, still-active selection must derive
    // the exact same target.
    h.send(Key::Char('c'));
    h.wait_for_text("README.md:4-5");

    h.send(Key::Esc);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("C-s save")
    });
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
