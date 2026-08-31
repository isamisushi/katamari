//! Issue #28: `C-w`/`M-Backspace` word deletion in the comment-compose
//! overlay, driven through the real compiled binary and a real (fake) PTY
//! — the one call site among `text_input::recognize`'s four that most
//! benefits from an end-to-end check, since it's the multi-line
//! `ComposeBuffer` path (`delete_previous_word`'s own line-merge branch,
//! not just `LineInput`'s single-line one already covered in-process by
//! `ui::compose`'s unit tests).
//!
//! `basic_repo`'s row layout (see `tests/e2e/range_comment.rs`'s own docs
//! for the identical fixture): three `j` presses from row 0 (a file-header
//! row `App::comment_target` refuses — see
//! `tests/e2e/skill_install.rs::save_a_comment`'s docs) lands on a real
//! `Context` line eligible for `c`.

use crate::support::{Harness, Key, KittyMode, SpawnOptions, fixture};
use std::time::Duration;

/// Opens the compose overlay on `basic_repo`'s third row, the same setup
/// `tests/e2e/skill_install.rs::save_a_comment` uses.
fn open_compose(h: &Harness) {
    h.wait_for_text("README.md");
    for _ in 0..3 {
        h.send(Key::Char('j'));
    }
    h.send(Key::Char('c'));
    h.wait_for_text("C-s save"); // the compose overlay's own hint line
}

#[test]
fn ctrl_w_deletes_the_previous_word_and_the_saved_comment_reflects_it() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    open_compose(&h);

    for c in "hello world".chars() {
        h.send(Key::Char(c));
    }
    h.wait_for_text("hello world");

    h.send(Key::CtrlW);
    h.wait_until(Duration::from_secs(2), |screen| {
        let contents = screen.contents();
        contents.contains("hello") && !contents.contains("world")
    });

    h.send(Key::CtrlS);
    h.wait_for_text("comment: saved");

    // The saved comment's body renders under the commented line — proof
    // the word deletion landed in the buffer that actually got persisted,
    // not just in the overlay's live preview.
    let contents = h.screen_contents();
    assert!(
        contents.contains("hello"),
        "saved comment must keep \"hello\"; screen:\n{contents}"
    );
    assert!(
        !contents.contains("world"),
        "C-w must have deleted \"world\" before save; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn ctrl_w_at_a_line_start_merges_into_the_previous_line_not_a_no_op() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    open_compose(&h);

    for c in "first".chars() {
        h.send(Key::Char(c));
    }
    h.send(Key::Enter); // second, empty line — cursor at its col 0
    h.wait_for_text("first");

    // At col 0 of a line past the first, `C-w` has no "word" behind it on
    // *this* line — `ComposeBuffer::delete_previous_word` falls back to
    // the same line-merge `backspace` does there, not a no-op.
    h.send(Key::CtrlW);

    h.send(Key::CtrlS);
    h.wait_for_text("comment: saved");
    let contents = h.screen_contents();
    assert!(
        contents.contains("first"),
        "the merged line's text must survive; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn alt_backspace_deletes_the_previous_word_under_kitty_mode() {
    let repo = fixture::basic_repo();
    // `SpawnOptions::default()` already negotiates `KittyMode::Supported`
    // (see its own `Default` impl) — spelled out here anyway since this
    // test's whole point is proving `Key::AltBackspace`'s kitty CSI-u
    // encoding round-trips through the real binary, not just the legacy
    // `ESC` + DEL form `Key::AltBackspace`'s docs describe as the fallback.
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            kitty_mode: KittyMode::Supported,
            ..Default::default()
        },
    );
    open_compose(&h);

    for c in "hello world".chars() {
        h.send(Key::Char(c));
    }
    h.wait_for_text("hello world");

    h.send(Key::AltBackspace);
    h.wait_until(Duration::from_secs(2), |screen| {
        let contents = screen.contents();
        contents.contains("hello") && !contents.contains("world")
    });

    h.send(Key::Esc); // discard — this test only cares about the live buffer
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
